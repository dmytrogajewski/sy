//! Local coding-agent launch orchestration for managed Spark inference.

use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    cli::{LaunchArgs, LaunchIntegration},
    client::{self, ClientError, SparkClient},
    wire::{
        InstanceDocument, ModelDocument, ModelListDocument, ServeAdmissionRequest, ServeRequest,
        TokenCreateRequest, TokenDocument, TokenScope,
    },
    EXIT_INTERNAL, EXIT_REJECTED, EXIT_USAGE,
};

const LAUNCH_STATE_SCHEMA: &str = "sy.spark.launch-state/v1";
const LAUNCH_PLAN_SCHEMA: &str = "sy.spark.launch-plan/v1";
const OWNED_MARKER: &str = "owned-by: sy spark launch";
const INFERENCE_CONCURRENCY: u32 = 8;
const CODEX_PROFILE: &str = "sy-spark-launch";
const CLAUDE_WIRE_MODEL: &str = "claude-sonnet-4-5";

#[derive(Debug, Clone, Serialize)]
struct LaunchPlan {
    schema: &'static str,
    host: String,
    integration: String,
    requested_model: String,
    model_id: String,
    model: String,
    instance: String,
    endpoint: Option<String>,
    reused_instance: bool,
    action: &'static str,
    token_id: Option<String>,
    extra_argument_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchState {
    schema: String,
    #[serde(default)]
    hosts: BTreeMap<String, HostLaunchState>,
}

impl Default for LaunchState {
    fn default() -> Self {
        Self {
            schema: LAUNCH_STATE_SCHEMA.into(),
            hosts: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostLaunchState {
    inference_token_id: Option<String>,
    #[serde(default)]
    integrations: BTreeMap<String, SavedSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSelection {
    model_reference: String,
    model_id: String,
    instance: String,
}

struct LaunchSecret(String);

struct ReadyLaunch<'a> {
    config_dir: &'a Path,
    host: &'a str,
    instance: &'a InstanceDocument,
    model: &'a ModelDocument,
    token: &'a LaunchSecret,
}

impl LaunchSecret {
    fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for LaunchSecret {
    fn drop(&mut self) {
        let len = self.0.len();
        self.0.replace_range(.., &"0".repeat(len));
        self.0.clear();
    }
}

pub fn run(host: &str, config_dir: &Path, args: LaunchArgs) -> Result<(), ClientError> {
    validate_args(&args)?;
    if args.restore {
        let _state_lock = acquire_state_lock(config_dir)?;
        return restore(host, config_dir, args.integration);
    }
    let state_lock = if args.dry_run {
        None
    } else {
        Some(acquire_state_lock(config_dir)?)
    };

    let mut state = read_state(config_dir)?;
    let saved = state
        .hosts
        .get(host)
        .and_then(|value| value.integrations.get(args.integration.as_str()))
        .cloned();
    let client = SparkClient::load(config_dir, host)?;
    let models = client.list_models()?;
    let model_reference = choose_model_reference(args.model.as_deref(), saved.as_ref(), &models)?;
    let instances = client.instances()?;
    let (model, selected_name) =
        resolve_launch_model(&models.models, &instances.instances, &model_reference)?;
    let owned_name = selected_name.unwrap_or_else(|| launch_instance_name(&model));
    let mut instance = resolve_instance(
        &instances.instances,
        &model,
        saved.as_ref().map(|value| value.instance.as_str()),
        &owned_name,
    )?;
    let reused_instance = instance.is_some();

    if args.dry_run {
        if instance.is_none() {
            let report = client.admission_plan(
                &uuid::Uuid::new_v4().to_string(),
                &ServeAdmissionRequest {
                    model: serve_model_reference(&model).into(),
                    name: Some(owned_name.clone()),
                    dry_run: true,
                },
            )?;
            if !report.admitted {
                return Err(failure(
                    EXIT_REJECTED,
                    "Spark resource admission rejected the launch model",
                ));
            }
        }
        return render_plan(
            &LaunchPlan {
                schema: LAUNCH_PLAN_SCHEMA,
                host: host.into(),
                integration: args.integration.as_str().into(),
                requested_model: model_reference,
                model_id: model.id,
                model: model.canonical,
                instance: instance
                    .as_ref()
                    .map(|value| value.name.clone())
                    .unwrap_or(owned_name),
                endpoint: instance.and_then(|value| value.endpoint),
                reused_instance,
                action: if reused_instance { "reuse" } else { "serve" },
                token_id: None,
                extra_argument_count: args.extra_args.len(),
            },
            args.json,
        );
    }

    if instance.is_none() {
        let operation = client.serve(
            &uuid::Uuid::new_v4().to_string(),
            &ServeRequest {
                model: serve_model_reference(&model).into(),
                name: Some(owned_name.clone()),
                dry_run: false,
            },
        )?;
        client.follow_operation(&operation.id, 0)?;
        instance = Some(
            client
                .instances()?
                .instances
                .into_iter()
                .find(|value| {
                    value.name == owned_name
                        && value.model_id == model.id
                        && value.healthy
                        && value.endpoint.is_some()
                })
                .ok_or_else(|| {
                    failure(
                        EXIT_REJECTED,
                        "Spark launch instance did not publish the exact requested model",
                    )
                })?,
        );
    }
    let instance = instance.ok_or_else(|| {
        failure(
            EXIT_INTERNAL,
            "Spark launch instance resolution ended without an instance",
        )
    })?;
    let (token_id, token) = ensure_inference_token(&client, host, config_dir, &mut state)?;
    configure_integration(args.integration, config_dir, host, &instance, &model)?;

    let host_state = state.hosts.entry(host.into()).or_default();
    host_state.inference_token_id = Some(token_id.clone());
    host_state.integrations.insert(
        args.integration.as_str().into(),
        SavedSelection {
            model_reference: model_reference.clone(),
            model_id: model.id.clone(),
            instance: instance.name.clone(),
        },
    );
    write_state(config_dir, &state)?;
    drop(state_lock);

    let plan = LaunchPlan {
        schema: LAUNCH_PLAN_SCHEMA,
        host: host.into(),
        integration: args.integration.as_str().into(),
        requested_model: model_reference,
        model_id: model.id.clone(),
        model: model.canonical.clone(),
        instance: instance.name.clone(),
        endpoint: instance.endpoint.clone(),
        reused_instance,
        action: if args.configure {
            "configure"
        } else {
            "launch"
        },
        token_id: Some(token_id),
        extra_argument_count: args.extra_args.len(),
    };
    if args.configure {
        return render_plan(&plan, args.json);
    }

    eprintln!(
        "Launching {} with {} on Spark instance {}",
        args.integration.as_str(),
        model.canonical,
        instance.name
    );
    launch_child(
        args.integration,
        args.yes,
        ReadyLaunch {
            config_dir,
            host,
            instance: &instance,
            model: &model,
            token: &token,
        },
        &args.extra_args,
    )
}

fn validate_args(args: &LaunchArgs) -> Result<(), ClientError> {
    if args.restore
        && (args.model.is_some()
            || args.configure
            || args.dry_run
            || args.json
            || args.yes
            || !args.extra_args.is_empty())
    {
        return Err(usage(
            "--restore cannot be combined with model, config, yes, dry-run, json, or agent arguments",
        ));
    }
    if args.json && !args.dry_run && !args.configure {
        return Err(usage(
            "--json requires --dry-run or --config because the launched agent owns stdout",
        ));
    }
    Ok(())
}

fn choose_model_reference(
    explicit: Option<&str>,
    saved: Option<&SavedSelection>,
    models: &ModelListDocument,
) -> Result<String, ClientError> {
    if let Some(value) = explicit.filter(|value| !value.trim().is_empty()) {
        return Ok(value.into());
    }
    if let Some(value) = saved {
        if models.models.iter().any(|model| model.id == value.model_id) {
            return Ok(value.model_reference.clone());
        }
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(usage(
            "launch requires --model when no usable saved selection exists",
        ));
    }
    select_model(models)
}

fn select_model(models: &ModelListDocument) -> Result<String, ClientError> {
    if models.models.is_empty() {
        return Err(usage(
            "no verified Spark models are installed; run `sy spark <host> download <repository> --revision <commit> --alias <name>`",
        ));
    }
    eprintln!("Select a verified Spark model:");
    for (index, model) in models.models.iter().enumerate() {
        eprintln!(
            "  {}) {}",
            index + 1,
            model.aliases.first().unwrap_or(&model.canonical)
        );
    }
    eprint!("> ");
    io::stderr()
        .flush()
        .map_err(|_| usage("could not render model selection prompt"))?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|_| usage("could not read model selection"))?;
    let index = line
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|value| (1..=models.models.len()).contains(value))
        .ok_or_else(|| usage("model selection is invalid"))?;
    let model = &models.models[index - 1];
    Ok(model
        .aliases
        .first()
        .cloned()
        .unwrap_or_else(|| model.canonical.clone()))
}

fn resolve_model(models: &[ModelDocument], reference: &str) -> Result<ModelDocument, ClientError> {
    let matches = models
        .iter()
        .filter(|model| {
            model.id == reference
                || model.canonical == reference
                || model.repository == reference
                || model.aliases.iter().any(|alias| alias == reference)
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [model] => Ok(model.clone()),
        [] => Err(usage(format!(
            "Spark model {reference:?} is not installed; run `sy spark <host> download <repository> --revision <commit> --alias {reference}`"
        ))),
        many => Err(usage(format!(
            "Spark model {reference:?} is ambiguous across: {}",
            many.iter()
                .map(|model| model.canonical.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn resolve_launch_model(
    models: &[ModelDocument],
    instances: &[InstanceDocument],
    reference: &str,
) -> Result<(ModelDocument, Option<String>), ClientError> {
    let instance = instances.iter().find(|value| value.name == reference);
    let model_reference = instance
        .map(|value| value.model_id.as_str())
        .unwrap_or(reference);
    Ok((
        resolve_model(models, model_reference)?,
        instance.map(|value| value.name.clone()),
    ))
}

fn serve_model_reference(model: &ModelDocument) -> &str {
    &model.id
}

fn resolve_instance(
    instances: &[InstanceDocument],
    model: &ModelDocument,
    saved_name: Option<&str>,
    owned_name: &str,
) -> Result<Option<InstanceDocument>, ClientError> {
    let healthy = instances
        .iter()
        .filter(|value| value.model_id == model.id && value.healthy && value.endpoint.is_some())
        .cloned()
        .collect::<Vec<_>>();
    if let Some(saved) = saved_name {
        if let Some(instance) = healthy.iter().find(|value| value.name == saved) {
            return Ok(Some(instance.clone()));
        }
    }
    if let Some(instance) = healthy.iter().find(|value| value.name == owned_name) {
        return Ok(Some(instance.clone()));
    }
    match healthy.as_slice() {
        [] => Ok(None),
        [instance] => Ok(Some(instance.clone())),
        many => Err(usage(format!(
            "multiple healthy Spark instances serve the requested model: {}",
            many.iter()
                .map(|value| value.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn launch_instance_name(model: &ModelDocument) -> String {
    let mut slug = model
        .aliases
        .first()
        .unwrap_or(&model.repository)
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() {
                value.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    slug = slug.trim_matches('-').chars().take(36).collect();
    if slug.is_empty() {
        slug = "model".into();
    }
    let digest = format!("{:x}", Sha256::digest(model.id.as_bytes()));
    format!("launch-{slug}-{}", &digest[..8])
}

fn ensure_inference_token(
    client: &SparkClient,
    host: &str,
    config_dir: &Path,
    state: &mut LaunchState,
) -> Result<(String, LaunchSecret), ClientError> {
    let credential_path = launch_credential_path(config_dir, host);
    let saved_id = state
        .hosts
        .get(host)
        .and_then(|value| value.inference_token_id.as_deref());
    let existing = read_private_optional(&credential_path)?;
    let existing_id = existing.as_deref().and_then(bearer_token_id);
    let tokens = client.list_tokens()?;
    if let (Some(saved_id), Some(existing_id), Some(secret)) =
        (saved_id, existing_id, existing.as_deref())
    {
        if saved_id == existing_id
            && tokens
                .tokens
                .iter()
                .any(|token| usable_launch_token(token, existing_id))
        {
            return Ok((existing_id.into(), LaunchSecret(secret.into())));
        }
    }

    if let Some(old_id) = existing_id.or(saved_id) {
        if tokens
            .tokens
            .iter()
            .any(|token| owned_launch_token(token, old_id))
        {
            let operation = client.revoke_token(old_id, &uuid::Uuid::new_v4().to_string())?;
            if !operation.state.is_terminal() {
                client.follow_operation(&operation.id, 0)?;
            }
        }
    }

    let local_name = env::var("HOSTNAME").unwrap_or_else(|_| "workstation".into());
    let created = client.create_token(
        &uuid::Uuid::new_v4().to_string(),
        &TokenCreateRequest {
            name: format!("sy-launch@{local_name}"),
            scopes: vec![TokenScope::Inference],
            allowed_cidrs: Vec::new(),
            expires_at: None,
            max_concurrent_inference: INFERENCE_CONCURRENCY,
        },
    )?;
    if !created.operation.state.is_terminal() {
        client.follow_operation(&created.operation.id, 0)?;
    }
    let bearer = created.bearer_token.ok_or_else(|| {
        failure(
            EXIT_INTERNAL,
            "Spark did not return the created launch token",
        )
    })?;
    if bearer_token_id(&bearer) != Some(created.token.id.as_str()) {
        return Err(failure(
            EXIT_INTERNAL,
            "Spark returned inconsistent launch token metadata",
        ));
    }
    write_private_atomic(&credential_path, bearer.as_bytes())?;
    Ok((created.token.id, LaunchSecret(bearer)))
}

fn usable_launch_token(token: &TokenDocument, id: &str) -> bool {
    token.id == id
        && token.revoked_at.is_none()
        && token.scopes == [TokenScope::Inference]
        && token.max_concurrent_inference == INFERENCE_CONCURRENCY
        && token.name.starts_with("sy-launch@")
}

fn owned_launch_token(token: &TokenDocument, id: &str) -> bool {
    token.id == id && token.name.starts_with("sy-launch@")
}

fn bearer_token_id(value: &str) -> Option<&str> {
    let value = value.trim();
    let rest = value.strip_prefix("sy_")?;
    let (id, secret) = rest.split_once('_')?;
    (id.len() == 26
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && secret.len() == 64
        && secret.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(id)
}

fn configure_integration(
    integration: LaunchIntegration,
    config_dir: &Path,
    host: &str,
    instance: &InstanceDocument,
    model: &ModelDocument,
) -> Result<(), ClientError> {
    if integration != LaunchIntegration::Codex {
        return Ok(());
    }
    let config = client::codex_client_config(config_dir, host, &instance.name, &model.canonical)?;
    let home = codex_home()?;
    let profile = home.join(format!("{CODEX_PROFILE}.config.toml"));
    let catalog = home.join(format!("{CODEX_PROFILE}-models.json"));
    let profile_text = format!("# {OWNED_MARKER}\n{}", config.toml);
    if instance.context_window == 0 {
        return Err(failure(
            EXIT_REJECTED,
            "Spark instance has no declared context window; restart it with the current engine configuration",
        ));
    }
    let catalog_value = codex_catalog(model, instance.context_window);
    let catalog_text = serde_json::to_vec_pretty(&catalog_value)
        .map_err(|_| failure(EXIT_INTERNAL, "could not encode Codex model catalog"))?;
    write_private_atomic(&profile, profile_text.as_bytes())?;
    write_private_atomic(&catalog, &catalog_text)
}

fn codex_catalog(model: &ModelDocument, context_window: u64) -> Value {
    serde_json::json!({
        "owned_by": OWNED_MARKER,
        "models": [{
            "slug": model.canonical,
            "display_name": model.aliases.first().unwrap_or(&model.repository),
            "description": model.canonical,
            "context_window": context_window,
            "max_context_window": context_window,
            "effective_context_window_percent": 100,
            "shell_type": "default",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 0,
            "additional_speed_tiers": [],
            "service_tiers": [],
            "truncation_policy": { "mode": "bytes", "limit": 10000 },
            "input_modalities": ["text"],
            "base_instructions": "",
            "default_reasoning_summary": "none",
            "support_verbosity": true,
            "default_verbosity": "low",
            "supports_parallel_tool_calls": false,
            "supports_reasoning_summaries": false,
            "supported_reasoning_levels": [],
            "experimental_supported_tools": [],
            "supports_search_tool": false,
            "web_search_tool_type": "text",
            "supports_image_detail_original": false,
            "use_responses_lite": false,
            "model_messages": null,
            "upgrade": null
        }]
    })
}

fn launch_child(
    integration: LaunchIntegration,
    yes: bool,
    launch: ReadyLaunch<'_>,
    extra_args: &[String],
) -> Result<(), ClientError> {
    let executable = ensure_executable(integration, yes)?;
    let mut command = Command::new(executable);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    match integration {
        LaunchIntegration::Claude => configure_claude_command(
            &mut command,
            launch.config_dir,
            launch.host,
            launch.instance,
            launch.model,
            launch.token,
            extra_args,
        )?,
        LaunchIntegration::Codex => configure_codex_command(
            &mut command,
            launch.config_dir,
            launch.host,
            launch.instance,
            launch.model,
            launch.token,
            extra_args,
        )?,
        LaunchIntegration::Opencode => configure_opencode_command(
            &mut command,
            launch.config_dir,
            launch.host,
            launch.instance,
            launch.model,
            launch.token,
            extra_args,
        )?,
    }
    let status = command
        .status()
        .map_err(|_| usage(format!("could not start {}", integration.as_str())))?;
    child_result(integration, status)
}

fn configure_claude_command(
    command: &mut Command,
    config_dir: &Path,
    host: &str,
    instance: &InstanceDocument,
    model: &ModelDocument,
    token: &LaunchSecret,
    extra_args: &[String],
) -> Result<(), ClientError> {
    let config =
        client::claude_code_client_config(config_dir, host, &instance.name, &model.canonical)?;
    command
        .arg("--model")
        .arg(CLAUDE_WIRE_MODEL)
        .args(extra_args)
        .env_remove("ANTHROPIC_API_KEY")
        .env("ANTHROPIC_AUTH_TOKEN", token.expose())
        .env("ANTHROPIC_BASE_URL", config.base_url)
        .env("ANTHROPIC_MODEL", CLAUDE_WIRE_MODEL)
        .env("ANTHROPIC_DEFAULT_OPUS_MODEL", CLAUDE_WIRE_MODEL)
        .env("ANTHROPIC_DEFAULT_SONNET_MODEL", CLAUDE_WIRE_MODEL)
        .env("ANTHROPIC_DEFAULT_HAIKU_MODEL", CLAUDE_WIRE_MODEL)
        .env("CLAUDE_CODE_SUBAGENT_MODEL", CLAUDE_WIRE_MODEL)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1")
        .env("CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_FEEDBACK_COMMAND", "1")
        .env("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1")
        .env("NODE_EXTRA_CA_CERTS", config.ca_path);
    Ok(())
}

fn configure_codex_command(
    command: &mut Command,
    config_dir: &Path,
    host: &str,
    instance: &InstanceDocument,
    model: &ModelDocument,
    token: &LaunchSecret,
    extra_args: &[String],
) -> Result<(), ClientError> {
    validate_codex_extra_args(extra_args)?;
    let config = client::codex_client_config(config_dir, host, &instance.name, &model.canonical)?;
    let catalog = codex_home()?.join(format!("{CODEX_PROFILE}-models.json"));
    command.arg("--profile").arg(CODEX_PROFILE);
    let mut overrides = vec![
        format!("model_provider={:?}", provider_name(&instance.name)),
        format!(
            "model_providers.{}.name={:?}",
            provider_name(&instance.name),
            format!("sy Spark {}", instance.name)
        ),
        format!(
            "model_providers.{}.base_url={:?}",
            provider_name(&instance.name),
            config.base_url
        ),
        format!(
            "model_providers.{}.env_key={:?}",
            provider_name(&instance.name),
            "SY_SPARK_INFERENCE_TOKEN"
        ),
        format!(
            "model_providers.{}.wire_api={:?}",
            provider_name(&instance.name),
            "responses"
        ),
        format!("model_catalog_json={:?}", catalog),
    ];
    if let Some(effort) = &instance.default_reasoning_effort {
        overrides.push(format!("model_reasoning_effort={effort:?}"));
    }
    for value in overrides {
        command.arg("-c").arg(value);
    }
    command
        .arg("-m")
        .arg(&model.canonical)
        .args(extra_args)
        .env("SY_SPARK_INFERENCE_TOKEN", token.expose())
        .env("OPENAI_API_KEY", token.expose())
        .env("SSL_CERT_FILE", config.ca_path);
    Ok(())
}

fn configure_opencode_command(
    command: &mut Command,
    config_dir: &Path,
    host: &str,
    instance: &InstanceDocument,
    model: &ModelDocument,
    token: &LaunchSecret,
    extra_args: &[String],
) -> Result<(), ClientError> {
    let config = client::codex_client_config(config_dir, host, &instance.name, &model.canonical)?;
    let content = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "sy-spark": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "sy Spark",
                "options": {
                    "baseURL": config.base_url,
                    "apiKey": "{env:SY_SPARK_INFERENCE_TOKEN}"
                },
                "models": {
                    model.canonical.clone(): { "name": model.canonical }
                }
            }
        },
        "model": format!("sy-spark/{}", model.canonical)
    });
    command
        .args(extra_args)
        .env(
            "OPENCODE_CONFIG_CONTENT",
            serde_json::to_string(&content)
                .map_err(|_| failure(EXIT_INTERNAL, "could not encode OpenCode config"))?,
        )
        .env("SY_SPARK_INFERENCE_TOKEN", token.expose())
        .env("NODE_EXTRA_CA_CERTS", config.ca_path);
    Ok(())
}

fn provider_name(instance: &str) -> String {
    format!("sy_spark_{}", instance.replace(['-', '.'], "_"))
}

fn validate_codex_extra_args(args: &[String]) -> Result<(), ClientError> {
    for (index, arg) in args.iter().enumerate() {
        let conflicts = arg == "-p"
            || arg.starts_with("-p")
            || arg == "--profile"
            || arg.starts_with("--profile=")
            || arg == "-m"
            || (arg.starts_with("-m") && arg.len() > 2)
            || arg == "--model"
            || arg.starts_with("--model=")
            || ((arg == "-c" || arg == "--config")
                && args
                    .get(index + 1)
                    .is_some_and(|value| managed_codex_override(value)))
            || arg
                .strip_prefix("-c")
                .filter(|value| !value.is_empty())
                .is_some_and(managed_codex_override)
            || arg
                .strip_prefix("--config=")
                .is_some_and(managed_codex_override);
        if conflicts {
            return Err(usage(format!(
                "conflicting Codex agent argument {arg:?}: sy spark launch manages model and provider routing"
            )));
        }
    }
    Ok(())
}

fn managed_codex_override(value: &str) -> bool {
    let key = value
        .split_once('=')
        .map(|(key, _)| key)
        .unwrap_or(value)
        .trim()
        .trim_matches(['\'', '"']);
    matches!(
        key,
        "profile" | "model" | "model_provider" | "model_catalog_json"
    ) || key.starts_with("model_providers.")
}

fn ensure_executable(integration: LaunchIntegration, yes: bool) -> Result<PathBuf, ClientError> {
    if let Some(path) = find_executable(integration) {
        if integration == LaunchIntegration::Codex {
            check_codex_version(&path)?;
        }
        return Ok(path);
    }
    if integration == LaunchIntegration::Codex {
        return Err(usage(
            "codex is not installed; install with: npm install -g @openai/codex",
        ));
    }
    let approved = yes || confirm_install(integration)?;
    if !approved {
        return Err(usage(format!(
            "{} installation was not approved",
            integration.as_str()
        )));
    }
    let script = match integration {
        LaunchIntegration::Claude => "curl -fsSL https://claude.ai/install.sh | bash",
        LaunchIntegration::Opencode => {
            "set -o pipefail; curl -fsSL https://opencode.ai/install | bash"
        }
        LaunchIntegration::Codex => unreachable!(),
    };
    let status = Command::new("bash")
        .args(["-c", script])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|_| usage("could not start the fixed agent installer"))?;
    if !status.success() {
        return Err(usage(format!("{} installer failed", integration.as_str())));
    }
    find_executable(integration).ok_or_else(|| {
        usage(format!(
            "{} was installed but its executable is unavailable",
            integration.as_str()
        ))
    })
}

fn find_executable(integration: LaunchIntegration) -> Option<PathBuf> {
    let name = integration.as_str();
    if let Some(path) = env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
    }) {
        return Some(path);
    }
    let home = env::var_os("HOME").map(PathBuf::from)?;
    let fallbacks = match integration {
        LaunchIntegration::Claude => vec![
            home.join(".local/bin/claude"),
            home.join(".claude/local/claude"),
        ],
        LaunchIntegration::Opencode => vec![home.join(".opencode/bin/opencode")],
        LaunchIntegration::Codex => vec![home.join(".local/bin/codex")],
    };
    fallbacks.into_iter().find(|path| path.is_file())
}

fn confirm_install(integration: LaunchIntegration) -> Result<bool, ClientError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(usage(format!(
            "{} is not installed; rerun in a terminal or pass --yes",
            integration.as_str()
        )));
    }
    eprint!(
        "{} is not installed. Install now? [y/N] ",
        integration.as_str()
    );
    io::stderr()
        .flush()
        .map_err(|_| usage("could not render installation prompt"))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|_| usage("could not read installation confirmation"))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn check_codex_version(path: &Path) -> Result<(), ClientError> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|_| usage("could not read codex version"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text
        .split_whitespace()
        .last()
        .and_then(parse_version)
        .ok_or_else(|| usage("codex returned an incompatible version string"))?;
    if version < (0, 134, 0) {
        return Err(usage(
            "codex is too old; update with: npm update -g @openai/codex",
        ));
    }
    Ok(())
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let mut fields = value.trim_start_matches('v').split('.');
    let major = fields.next()?.parse().ok()?;
    let minor = fields.next()?.parse().ok()?;
    let patch = fields.next()?.split('-').next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn child_result(integration: LaunchIntegration, status: ExitStatus) -> Result<(), ClientError> {
    if status.success() {
        return Ok(());
    }
    let code = status
        .code()
        .filter(|code| (1..=125).contains(code))
        .unwrap_or(1);
    Err(failure(
        code,
        format!("{} exited with status {code}", integration.as_str()),
    ))
}

fn restore(
    host: &str,
    config_dir: &Path,
    integration: LaunchIntegration,
) -> Result<(), ClientError> {
    if integration != LaunchIntegration::Codex {
        return Err(usage(format!(
            "{} does not create persistent client configuration and does not support --restore",
            integration.as_str()
        )));
    }
    let home = codex_home()?;
    remove_owned_text(&home.join(format!("{CODEX_PROFILE}.config.toml")))?;
    remove_owned_json(&home.join(format!("{CODEX_PROFILE}-models.json")))?;
    let mut state = read_state(config_dir)?;
    if let Some(host_state) = state.hosts.get_mut(host) {
        host_state.integrations.remove(integration.as_str());
    }
    write_state(config_dir, &state)?;
    println!("Codex Spark launch configuration removed.");
    Ok(())
}

fn remove_owned_text(path: &Path) -> Result<(), ClientError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(usage("could not read Codex launch profile")),
    };
    if !text
        .lines()
        .next()
        .is_some_and(|line| line.contains(OWNED_MARKER))
    {
        return Err(usage("refusing to remove an unowned Codex profile"));
    }
    fs::remove_file(path).map_err(|_| usage("could not remove Codex launch profile"))
}

fn remove_owned_json(path: &Path) -> Result<(), ClientError> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(usage("could not read Codex launch catalog")),
    };
    let owned = serde_json::from_slice::<serde_json::Value>(&data)
        .ok()
        .and_then(|value| {
            value
                .get("owned_by")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .is_some_and(|value| value == OWNED_MARKER);
    if !owned {
        return Err(usage("refusing to remove an unowned Codex model catalog"));
    }
    fs::remove_file(path).map_err(|_| usage("could not remove Codex launch catalog"))
}

fn codex_home() -> Result<PathBuf, ClientError> {
    if let Some(path) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".codex"))
        .ok_or_else(|| usage("HOME or CODEX_HOME is required for Codex launch"))
}

fn state_path(config_dir: &Path) -> PathBuf {
    config_dir.join("spark-launch.toml")
}

#[cfg(unix)]
fn acquire_state_lock(config_dir: &Path) -> Result<nix::fcntl::Flock<fs::File>, ClientError> {
    use nix::fcntl::{Flock, FlockArg};
    use std::os::unix::fs::OpenOptionsExt;

    fs::create_dir_all(config_dir)
        .map_err(|_| usage("could not create Spark launch configuration directory"))?;
    let path = config_dir.join("spark-launch.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|_| usage("could not open Spark launch state lock"))?;
    enforce_private_metadata(
        &file
            .metadata()
            .map_err(|_| usage("could not inspect Spark launch state lock"))?,
    )?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|_| {
        failure(
            EXIT_REJECTED,
            "another sy spark launch is updating local launch state",
        )
    })
}

#[cfg(not(unix))]
fn acquire_state_lock(_: &Path) -> Result<(), ClientError> {
    Err(usage("Spark launch state locking requires Unix"))
}

fn launch_credential_path(config_dir: &Path, host: &str) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(host.as_bytes()));
    config_dir
        .join("credentials/spark")
        .join(format!("launch-{}", &digest[..16]))
}

fn read_state(config_dir: &Path) -> Result<LaunchState, ClientError> {
    let path = state_path(config_dir);
    let text = match read_private_optional(&path)? {
        Some(text) => text,
        None => return Ok(LaunchState::default()),
    };
    let state: LaunchState = toml::from_str(&text)
        .map_err(|_| usage("Spark launch state is malformed; move it aside and retry"))?;
    if state.schema != LAUNCH_STATE_SCHEMA {
        return Err(usage("Spark launch state schema is unsupported"));
    }
    Ok(state)
}

fn write_state(config_dir: &Path, state: &LaunchState) -> Result<(), ClientError> {
    let text = toml::to_string_pretty(state)
        .map_err(|_| failure(EXIT_INTERNAL, "could not encode Spark launch state"))?;
    write_private_atomic(&state_path(config_dir), text.as_bytes())
}

fn read_private_optional(path: &Path) -> Result<Option<String>, ClientError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(usage("protected Spark launch file is unavailable")),
    };
    enforce_private_metadata(&metadata)?;
    let mut file =
        fs::File::open(path).map_err(|_| usage("protected Spark launch file is unavailable"))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|_| usage("protected Spark launch file is unreadable"))?;
    Ok(Some(text.trim().to_owned()))
}

#[cfg(unix)]
fn enforce_private_metadata(metadata: &fs::Metadata) -> Result<(), ClientError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(usage(
            "protected Spark launch file permissions must be 0600",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_private_metadata(_: &fs::Metadata) -> Result<(), ClientError> {
    Err(usage("Spark launch credentials require Unix permissions"))
}

#[cfg(unix)]
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), ClientError> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .ok_or_else(|| usage("protected Spark launch path has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|_| usage("could not create protected Spark launch directory"))?;
    let temporary = path.with_extension(format!("new-{}", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| usage("could not stage protected Spark launch file"))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| usage("could not persist protected Spark launch file"))?;
    fs::rename(&temporary, path)
        .map_err(|_| usage("could not atomically install protected Spark launch file"))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| usage("could not persist protected Spark launch directory"))
}

#[cfg(not(unix))]
fn write_private_atomic(_: &Path, _: &[u8]) -> Result<(), ClientError> {
    Err(usage("Spark launch credentials require Unix permissions"))
}

fn render_plan(plan: &LaunchPlan, json: bool) -> Result<(), ClientError> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(plan)
                .map_err(|_| failure(EXIT_INTERNAL, "could not encode Spark launch plan"))?
        );
    } else {
        println!(
            "{} {} with {} on {} ({})",
            plan.action, plan.integration, plan.model, plan.instance, plan.host
        );
    }
    Ok(())
}

fn usage(message: impl Into<String>) -> ClientError {
    failure(EXIT_USAGE, message)
}

fn failure(code: i32, message: impl Into<String>) -> ClientError {
    ClientError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spark::wire::{
        InstanceDesiredState, InstanceObservedState, RecipeResourceEnvelopeDocument,
    };
    use std::ffi::OsStr;

    fn client_config(root: &Path) {
        fs::write(
            root.join("spark.toml"),
            "[hosts.dgx]\nurl = \"https://127.0.0.1:9843\"\nca_cert_sha256 = \"sha256:test\"\ncredential = \"spark/dgx\"\nrequest_timeout_seconds = 30\n",
        )
        .unwrap();
    }

    fn command_env(command: &Command, key: &str) -> Option<Option<String>> {
        command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(key))
            .map(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    }

    fn model(id: &str, alias: &str) -> ModelDocument {
        ModelDocument {
            schema: "sy.spark.model/v1".into(),
            id: id.into(),
            canonical: format!("huggingface:org/{alias}@commit"),
            repository: format!("org/{alias}"),
            commit: "commit".into(),
            snapshot: "/models/snapshot".into(),
            artifacts: None,
            logical_bytes: 1,
            unique_bytes: 1,
            aliases: vec![alias.into()],
            active_instances: Vec::new(),
            transport: "hf-native".into(),
            verified_at: "2026-08-25T00:00:00Z".into(),
            gated: false,
            license: Some("apache-2.0".into()),
        }
    }

    fn artifacts() -> crate::spark::wire::ModelArtifactsDocument {
        serde_json::from_str(r#"{"schema":"sy.spark.model-artifacts/v2","format":"gguf","primary":{"path":"model.gguf","bytes":8,"sha256":null},"auxiliary":[],"quantization":"Q4_K_XL","capabilities":["text_generation"],"configured_alias":null}"#).unwrap()
    }

    fn instance(name: &str, model_id: &str) -> InstanceDocument {
        let artifacts = artifacts();
        InstanceDocument {
            schema: "sy.spark.instance/v2".into(),
            id: format!("i_{name}"),
            name: name.into(),
            model_id: model_id.into(),
            model: "huggingface:org/model@commit".into(),
            model_commit: "commit".into(),
            engine_id: "llama-cpp".into(),
            engine_fingerprint: format!("sha256:{}", "a".repeat(64)),
            artifact_fingerprint: format!("sha256:{}", "b".repeat(64)),
            artifacts,
            objective: "agent".into(),
            resources: RecipeResourceEnvelopeDocument {
                image_bytes: 1,
                startup_peak_bytes: 1,
                steady_peak_bytes: 1,
                compile_cache_bytes: 0,
            },
            context_window: 65_536,
            default_reasoning_effort: None,
            generation: 1,
            desired: InstanceDesiredState::Running,
            observed: InstanceObservedState::Healthy,
            endpoint: Some(format!("/openai/{name}/v1")),
            healthy: true,
            started_at: Some("2026-08-25T00:00:00Z".into()),
            startup_milliseconds: Some(1),
            last_failure: None,
            restart_failures: 0,
            restart_suppressed: false,
            quarantine: None,
        }
    }

    #[test]
    fn exact_alias_resolves_one_verified_model() {
        let expected = model("m_one", "ornith-1.5:9b");
        let actual = resolve_model(std::slice::from_ref(&expected), "ornith-1.5:9b").unwrap();
        assert_eq!(actual.id, expected.id);
    }

    #[test]
    fn stopped_instance_name_resolves_its_installed_model() {
        let expected = model("m_one", "ornith-1.5:35b");
        let mut stopped = instance("ornith-1-5-35b", &expected.id);
        stopped.healthy = false;
        let (actual, name) = resolve_launch_model(
            std::slice::from_ref(&expected),
            std::slice::from_ref(&stopped),
            &stopped.name,
        )
        .unwrap();
        assert_eq!((actual.id, name), (expected.id, Some(stopped.name)));
    }

    #[test]
    fn stopped_instance_serve_uses_resolved_model_id() {
        let model = model("m_one", "ornith-1.5:35b");
        assert_eq!(serve_model_reference(&model), "m_one");
    }

    #[test]
    fn saved_instance_wins_over_other_healthy_instances() {
        let model = model("m_one", "ornith");
        let instances = [instance("other", &model.id), instance("saved", &model.id)];
        let actual = resolve_instance(&instances, &model, Some("saved"), "owned")
            .unwrap()
            .unwrap();
        assert_eq!(actual.name, "saved");
    }

    #[test]
    fn launch_name_is_stable_and_safe() {
        let name = launch_instance_name(&model("m_one", "Ornith-1.5:9B"));
        assert!(name.starts_with("launch-ornith-1-5-9b-"));
    }

    #[test]
    fn codex_conflicting_model_argument_is_rejected() {
        let error = validate_codex_extra_args(&["--model".into(), "other".into()]).unwrap_err();
        assert_eq!(error.code, EXIT_USAGE);
    }

    #[test]
    fn claude_adapter_uses_native_route_exact_args_and_inference_secret() {
        let root = tempfile::tempdir().unwrap();
        client_config(root.path());
        let model = model("m_one", "ornith");
        let instance = instance("ornith", &model.id);
        let secret = LaunchSecret("inference-secret".into());
        let mut command = Command::new("claude");
        configure_claude_command(
            &mut command,
            root.path(),
            "dgx",
            &instance,
            &model,
            &secret,
            &["--permission-mode".into(), "plan".into()],
        )
        .unwrap();

        let args = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            ["--model", CLAUDE_WIRE_MODEL, "--permission-mode", "plan"]
        );
        assert_eq!(
            command_env(&command, "ANTHROPIC_BASE_URL")
                .unwrap()
                .unwrap(),
            "https://127.0.0.1:9843/anthropic/ornith"
        );
        assert_eq!(
            command_env(&command, "ANTHROPIC_AUTH_TOKEN")
                .unwrap()
                .unwrap(),
            "inference-secret"
        );
        assert_eq!(command_env(&command, "ANTHROPIC_API_KEY"), Some(None));
        assert_eq!(
            command_env(&command, "ANTHROPIC_MODEL").unwrap().unwrap(),
            CLAUDE_WIRE_MODEL
        );
        assert_eq!(
            command_env(&command, "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"),
            Some(Some("1".into()))
        );
    }

    #[test]
    fn codex_adapter_uses_the_instances_declarative_reasoning_effort() {
        let root = tempfile::tempdir().unwrap();
        client_config(root.path());
        let model = model("m_one", "ornith");
        let mut instance = instance("ornith", &model.id);
        instance.default_reasoning_effort = Some("medium".into());
        let mut command = Command::new("codex");

        configure_codex_command(
            &mut command,
            root.path(),
            "dgx",
            &instance,
            &model,
            &LaunchSecret("inference-secret".into()),
            &["exec".into(), "build".into()],
        )
        .unwrap();

        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["-c", "model_reasoning_effort=\"medium\""]));
    }

    #[test]
    fn opencode_adapter_uses_inline_provider_without_embedding_secret() {
        let root = tempfile::tempdir().unwrap();
        client_config(root.path());
        let model = model("m_one", "ornith");
        let instance = instance("ornith", &model.id);
        let secret = LaunchSecret("inference-secret".into());
        let mut command = Command::new("opencode");
        configure_opencode_command(
            &mut command,
            root.path(),
            "dgx",
            &instance,
            &model,
            &secret,
            &["run".into(), "hello world".into()],
        )
        .unwrap();

        let content = command_env(&command, "OPENCODE_CONFIG_CONTENT")
            .unwrap()
            .unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(config["model"], format!("sy-spark/{}", model.canonical));
        assert_eq!(
            config["provider"]["sy-spark"]["options"]["baseURL"],
            "https://127.0.0.1:9843/openai/ornith/v1"
        );
        assert_eq!(
            config["provider"]["sy-spark"]["options"]["apiKey"],
            "{env:SY_SPARK_INFERENCE_TOKEN}"
        );
        assert!(!content.contains("inference-secret"));
    }

    #[test]
    fn codex_catalog_matches_current_required_shape_without_secret() {
        let catalog = codex_catalog(&model("m_one", "ornith"), 262_144);
        assert_eq!(catalog["models"][0]["context_window"], 262_144);
        assert_eq!(catalog["models"][0]["max_context_window"], 262_144);
        assert_eq!(
            catalog["models"][0]["effective_context_window_percent"],
            100
        );
        assert_eq!(catalog["models"][0]["supported_in_api"], true);
        assert_eq!(catalog["models"][0]["shell_type"], "default");
        assert_eq!(catalog["models"][0]["input_modalities"][0], "text");
        assert_eq!(catalog["models"][0]["truncation_policy"]["mode"], "bytes");
        assert_eq!(catalog["models"][0]["supports_reasoning_summaries"], false);
        assert_eq!(catalog["models"][0]["default_reasoning_summary"], "none");
        assert_eq!(
            catalog["models"][0]["supported_reasoning_levels"],
            serde_json::json!([])
        );
        assert!(!catalog.to_string().contains("inference-secret"));
    }

    #[test]
    fn launch_token_parser_rejects_short_secrets() {
        assert_eq!(bearer_token_id("sy_01ABCDEFGHIJKLMNOPQRSTUVWX_short"), None);
    }

    #[test]
    fn absent_model_has_immutable_download_remediation() {
        let error = resolve_model(&[], "ornith-1.5:9b").unwrap_err();
        assert!(error.message.contains("--revision <commit>"));
    }

    #[test]
    fn ambiguous_alias_is_rejected() {
        let models = [model("m_one", "shared"), model("m_two", "shared")];
        let error = resolve_model(&models, "shared").unwrap_err();
        assert!(error.message.contains("ambiguous"));
    }

    #[test]
    fn unrelated_multiple_healthy_instances_are_rejected() {
        let model = model("m_one", "ornith");
        let instances = [instance("one", &model.id), instance("two", &model.id)];
        let error = resolve_instance(&instances, &model, None, "owned").unwrap_err();
        assert!(error.message.contains("one, two"));
    }

    #[test]
    fn launch_state_is_private_and_contains_no_bearer() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let mut state = LaunchState::default();
        state.hosts.insert(
            "dgx".into(),
            HostLaunchState {
                inference_token_id: Some("01ABCDEFGHIJKLMNOPQRSTUVWX".into()),
                integrations: BTreeMap::new(),
            },
        );
        write_state(root.path(), &state).unwrap();
        let path = state_path(root.path());
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(
            (
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                contents.contains("secret")
            ),
            (0o600, false)
        );
    }

    #[test]
    fn concurrent_launch_state_writer_is_rejected_until_unlock() {
        let root = tempfile::tempdir().unwrap();
        let first = acquire_state_lock(root.path()).unwrap();
        assert_eq!(
            acquire_state_lock(root.path()).unwrap_err().code,
            EXIT_REJECTED
        );
        drop(first);
        assert!(acquire_state_lock(root.path()).is_ok());
    }

    #[test]
    fn only_exact_inference_token_is_usable() {
        let token = TokenDocument {
            schema: "sy.spark.token/v1".into(),
            id: "01ABCDEFGHIJKLMNOPQRSTUVWX".into(),
            name: "sy-launch@host".into(),
            scopes: vec![TokenScope::Inference],
            allowed_cidrs: Vec::new(),
            expires_at: None,
            max_concurrent_inference: INFERENCE_CONCURRENCY,
            created_at: "2026-08-25T00:00:00Z".into(),
            last_used_at: None,
            revoked_at: None,
        };
        assert!(usable_launch_token(&token, &token.id));
    }

    #[test]
    fn restore_flags_cannot_be_combined() {
        let args = LaunchArgs {
            integration: LaunchIntegration::Codex,
            model: Some("ornith".into()),
            configure: false,
            restore: true,
            yes: false,
            dry_run: false,
            json: false,
            config_dir: None,
            extra_args: Vec::new(),
        };
        assert_eq!(validate_args(&args).unwrap_err().code, EXIT_USAGE);
    }

    #[test]
    fn child_numeric_exit_status_is_preserved() {
        let status = Command::new("sh").args(["-c", "exit 7"]).status().unwrap();
        assert_eq!(
            child_result(LaunchIntegration::Claude, status)
                .unwrap_err()
                .code,
            7
        );
    }
}
