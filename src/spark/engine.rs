//! Runtime-loaded Spark inference-engine policy.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    gateway::{EmbeddingPolicy, GatewayProfile, VisionPolicy},
    wire::{
        ModelArtifactFormat, ModelArtifactRole, ModelArtifactsDocument,
        RecipeResourceEnvelopeDocument,
    },
    MAX_ENGINE_STARTUP_DEADLINE_SECONDS,
};

pub const ENGINE_SCHEMA: &str = "sy.spark.engine/v3";
const ALLOWED_PLACEHOLDERS: [&str; 5] = [
    "{model_snapshot}",
    "{model_file}",
    "{auxiliary_file}",
    "{served_model}",
    "{port}",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub schema: String,
    pub id: String,
    pub priority: u16,
    pub matcher: EngineMatcher,
    pub family: String,
    pub version: String,
    pub image_transport: EngineImageTransport,
    pub image_repository: String,
    pub image_digest: String,
    pub image_architecture: String,
    pub entrypoint: Vec<String>,
    pub arguments: Vec<String>,
    pub artifact_arguments: EngineArtifactArguments,
    pub model_mount: EngineModelMount,
    #[serde(default)]
    pub environment: Vec<String>,
    #[serde(default)]
    pub executable_cache_environment: Vec<String>,
    pub default_profile: String,
    pub model_cache_root: String,
    pub compile_cache_root: String,
    pub network: String,
    pub port: u16,
    pub run_as_uid: u32,
    pub pid_limit: u32,
    pub seccomp: String,
    pub ipc_mode: String,
    pub shm_size_bytes: u64,
    pub tmpfs: Vec<String>,
    pub startup_deadline_seconds: u64,
    pub health_method: String,
    pub health_path: String,
    #[serde(default)]
    pub health_body: Option<EngineHealthBody>,
    pub semantic_prompt: String,
    pub semantic_max_tokens: u32,
    pub resources: EngineResources,
    pub routes: Vec<EngineRoute>,
    pub profiles: Vec<EngineProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineHealthBody {
    pub json_pointer: String,
    pub equals: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineModelMount {
    ArtifactFiles,
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineImageTransport {
    Registry,
    Local,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineMatcher {
    pub formats: Vec<ModelArtifactFormat>,
    #[serde(default)]
    pub quantizations: Vec<String>,
    #[serde(default)]
    pub engine_profiles: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineArtifactArguments {
    pub primary: Vec<String>,
    #[serde(default)]
    pub bindings: Vec<EngineArtifactBinding>,
    #[serde(default)]
    pub ignored_roles: Vec<ModelArtifactRole>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineArtifactBinding {
    pub role: ModelArtifactRole,
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineResources {
    pub image_bytes: u64,
    pub startup_peak_bytes: u64,
    pub steady_peak_bytes: u64,
    pub compile_cache_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineRoute {
    pub method: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineProfile {
    pub id: String,
    pub model_types: Vec<String>,
    pub context_window: u64,
    #[serde(default)]
    pub native_responses: bool,
    #[serde(default)]
    pub native_response_timeout_seconds: u64,
    #[serde(default)]
    pub stream_idle_timeout_seconds: u64,
    pub arguments: Vec<String>,
    pub capabilities: Vec<String>,
    pub startup_protocol_probe: bool,
    #[serde(default)]
    pub resources: Option<EngineResources>,
    #[serde(default)]
    pub vision: Option<VisionPolicy>,
    #[serde(default)]
    pub embeddings: Option<EngineEmbeddingConfig>,
    #[serde(default)]
    pub sampling: EngineSampling,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineEmbeddingConfig {
    pub dimensions_from_model_config: bool,
    pub max_batch: usize,
    pub max_input_bytes: usize,
    pub normalized: bool,
    pub normalization_tolerance_ppm: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineSampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub min_p: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub repetition_penalty: Option<f64>,
    pub thinking_token_budget: Option<u64>,
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning_effort_map: BTreeMap<String, String>,
    #[serde(default)]
    pub chat_template_kwargs: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct EnginePolicy {
    config: EngineConfig,
    fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct EngineCatalog {
    policies: BTreeMap<String, EnginePolicy>,
}

impl EngineCatalog {
    pub fn load(directory: &Path) -> Result<Self, String> {
        let entries = std::fs::read_dir(directory)
            .map_err(|error| format!("read Spark engine catalog: {error}"))?;
        let mut paths = entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read Spark engine catalog entry: {error}"))?;
        paths.sort();
        let mut files = Vec::new();
        for path in paths {
            if path.extension().and_then(|value| value.to_str()) != Some("toml") {
                return Err(format!(
                    "unexpected Spark engine catalog entry {}",
                    path.display()
                ));
            }
            let text = std::fs::read_to_string(&path).map_err(|error| {
                format!("read Spark engine declaration {}: {error}", path.display())
            })?;
            files.push((path, text));
        }
        Self::parse_files(
            files
                .iter()
                .map(|(path, text)| (path.display().to_string(), text.as_str())),
        )
    }

    pub fn parse_files<I, N, T>(files: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = (N, T)>,
        N: AsRef<str>,
        T: AsRef<str>,
    {
        let mut policies = BTreeMap::new();
        for (name, text) in files {
            let policy = EnginePolicy::parse(text.as_ref())
                .map_err(|error| format!("engine declaration {}: {error}", name.as_ref()))?;
            let id = policy.config.id.clone();
            if policies.insert(id.clone(), policy).is_some() {
                return Err(format!("duplicate Spark engine id {id}"));
            }
        }
        if policies.is_empty() {
            return Err("Spark engine catalog is empty".into());
        }
        Ok(Self { policies })
    }

    pub fn select(&self, artifacts: &ModelArtifactsDocument) -> Result<&EnginePolicy, String> {
        let matches = self
            .policies
            .values()
            .filter(|policy| policy.matches(artifacts))
            .collect::<Vec<_>>();
        let priority = matches
            .iter()
            .map(|policy| policy.config.priority)
            .max()
            .ok_or_else(|| "no Spark engine supports the artifact traits".to_owned())?;
        let winners = matches
            .into_iter()
            .filter(|policy| policy.config.priority == priority)
            .collect::<Vec<_>>();
        match winners.as_slice() {
            [winner] => {
                winner.artifact_argv(artifacts, |path| Ok(path.into()))?;
                Ok(*winner)
            }
            _ => Err(format!(
                "multiple Spark engines match at priority {priority}"
            )),
        }
    }

    pub fn get(&self, id: &str) -> Option<&EnginePolicy> {
        self.policies.get(id)
    }
}

impl EnginePolicy {
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: EngineConfig =
            toml::from_str(text).map_err(|error| format!("parse Spark engine policy: {error}"))?;
        validate(&config)?;
        Ok(Self {
            config,
            fingerprint: format!("sha256:{:x}", Sha256::digest(text.as_bytes())),
        })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn profile(&self, model_type: Option<&str>) -> &EngineProfile {
        model_type
            .and_then(|model_type| {
                self.config
                    .profiles
                    .iter()
                    .find(|profile| profile.model_types.iter().any(|item| item == model_type))
            })
            .or_else(|| {
                self.config
                    .profiles
                    .iter()
                    .find(|profile| profile.id == self.config.default_profile)
            })
            .expect("validated engine policy has a default profile")
    }

    pub fn profile_for(
        &self,
        model_type: Option<&str>,
        artifacts: &ModelArtifactsDocument,
    ) -> Result<&EngineProfile, String> {
        if let Some(profile) = artifacts.engine_profile.as_deref() {
            self.config
                .profiles
                .iter()
                .find(|candidate| candidate.id == profile)
                .ok_or_else(|| format!("engine has no configured profile {profile}"))
        } else if artifacts
            .capabilities
            .iter()
            .any(|value| value == "text_embeddings")
        {
            self.config
                .profiles
                .iter()
                .find(|profile| profile.embeddings.is_some())
                .ok_or_else(|| "engine has no embeddings profile".to_owned())
        } else {
            Ok(self.profile(model_type))
        }
    }

    pub fn image(&self) -> String {
        match self.config.image_transport {
            EngineImageTransport::Registry => format!(
                "{}@{}",
                self.config.image_repository, self.config.image_digest
            ),
            EngineImageTransport::Local => self.config.image_digest.clone(),
        }
    }

    pub fn resources_for(
        &self,
        model_type: Option<&str>,
        artifacts: &ModelArtifactsDocument,
    ) -> Result<RecipeResourceEnvelopeDocument, String> {
        let profile = self.profile_for(model_type, artifacts)?;
        Ok(resource_document(
            profile.resources.as_ref().unwrap_or(&self.config.resources),
        ))
    }

    #[cfg(test)]
    pub fn gateway_profile(&self, model_type: Option<&str>) -> GatewayProfile {
        let profile = self.profile(model_type);
        GatewayProfile {
            capabilities: profile.capabilities.iter().cloned().collect(),
            vision: profile.vision.clone(),
            embeddings: None,
            startup_protocol_probe: profile.startup_protocol_probe,
            native_responses: profile.native_responses,
            native_response_timeout_seconds: profile.native_response_timeout_seconds,
            stream_idle_timeout_seconds: profile.stream_idle_timeout_seconds,
            sampling: sampling_policy(&profile.sampling),
        }
    }

    pub fn gateway_profile_for(
        &self,
        model_type: Option<&str>,
        artifacts: &ModelArtifactsDocument,
        model_dimensions: Option<usize>,
    ) -> Result<GatewayProfile, String> {
        let profile = self.profile_for(model_type, artifacts)?;
        let embeddings = profile
            .embeddings
            .as_ref()
            .map(|config| {
                let dimensions = config
                    .dimensions_from_model_config
                    .then_some(model_dimensions)
                    .flatten()
                    .ok_or_else(|| "model config has no embedding dimensions".to_owned())?;
                Ok::<_, String>(EmbeddingPolicy {
                    dimensions,
                    max_batch: config.max_batch,
                    max_input_bytes: config.max_input_bytes,
                    normalized: config.normalized,
                    normalization_tolerance_ppm: config.normalization_tolerance_ppm,
                })
            })
            .transpose()?;
        let supports_vision = artifacts.capabilities.iter().any(|value| value == "vision");
        Ok(GatewayProfile {
            capabilities: profile
                .capabilities
                .iter()
                .filter(|capability| capability.as_str() != "vision" || supports_vision)
                .cloned()
                .collect(),
            vision: supports_vision.then(|| profile.vision.clone()).flatten(),
            embeddings,
            startup_protocol_probe: profile.startup_protocol_probe,
            native_responses: profile.native_responses,
            native_response_timeout_seconds: profile.native_response_timeout_seconds,
            stream_idle_timeout_seconds: profile.stream_idle_timeout_seconds,
            sampling: sampling_policy(&profile.sampling),
        })
    }

    pub fn artifact_argv<F>(
        &self,
        artifacts: &ModelArtifactsDocument,
        mut resolve: F,
    ) -> Result<Vec<String>, String>
    where
        F: FnMut(&str) -> Result<String, String>,
    {
        let mut argv = expand_file_arguments(
            &self.config.artifact_arguments.primary,
            "{model_file}",
            &resolve(&artifacts.primary.path)?,
        );
        for artifact in &artifacts.auxiliary {
            let binding = self
                .config
                .artifact_arguments
                .bindings
                .iter()
                .find(|binding| binding.role == artifact.role);
            if let Some(binding) = binding {
                argv.extend(expand_file_arguments(
                    &binding.arguments,
                    "{auxiliary_file}",
                    &resolve(&artifact.path)?,
                ));
            } else if !self
                .config
                .artifact_arguments
                .ignored_roles
                .contains(&artifact.role)
            {
                return Err("engine has no binding for a required artifact role".into());
            }
        }
        Ok(argv)
    }

    fn matches(&self, artifacts: &ModelArtifactsDocument) -> bool {
        let matcher = &self.config.matcher;
        matcher.formats.contains(&artifacts.format)
            && (matcher.engine_profiles.is_empty()
                || artifacts
                    .engine_profile
                    .as_ref()
                    .is_some_and(|profile| matcher.engine_profiles.contains(profile)))
            && (matcher.quantizations.is_empty()
                || artifacts.quantization.as_ref().is_some_and(|quantization| {
                    matcher
                        .quantizations
                        .iter()
                        .any(|value| value == quantization)
                }))
            && matcher
                .required_capabilities
                .iter()
                .all(|required| artifacts.capabilities.iter().any(|value| value == required))
    }
}

pub use super::wire::artifact_fingerprint;

fn sampling_policy(config: &EngineSampling) -> super::gateway::SamplingPolicy {
    let mut defaults = std::collections::BTreeMap::new();
    for (key, value) in [
        ("temperature", config.temperature),
        ("top_p", config.top_p),
        ("min_p", config.min_p),
        ("presence_penalty", config.presence_penalty),
        ("repetition_penalty", config.repetition_penalty),
    ] {
        if let Some(value) = value.and_then(serde_json::Number::from_f64) {
            defaults.insert(key.into(), value);
        }
    }
    if let Some(value) = config.top_k {
        defaults.insert("top_k".into(), value.into());
    }
    if let Some(value) = config.thinking_token_budget {
        defaults.insert("thinking_token_budget".into(), value.into());
    }
    super::gateway::SamplingPolicy {
        defaults,
        reasoning_effort_map: config.reasoning_effort_map.clone(),
        chat_template_kwargs: config.chat_template_kwargs.clone(),
    }
}

fn resource_document(resources: &EngineResources) -> RecipeResourceEnvelopeDocument {
    RecipeResourceEnvelopeDocument {
        image_bytes: resources.image_bytes,
        startup_peak_bytes: resources.startup_peak_bytes,
        steady_peak_bytes: resources.steady_peak_bytes,
        compile_cache_bytes: resources.compile_cache_bytes,
    }
}

fn valid_resources(resources: &EngineResources) -> bool {
    resources.image_bytes > 0
        && resources.startup_peak_bytes > 0
        && resources.steady_peak_bytes > 0
        && resources.compile_cache_bytes > 0
}

fn validate(config: &EngineConfig) -> Result<(), String> {
    if config.schema != ENGINE_SCHEMA {
        return Err("unsupported Spark engine policy schema".into());
    }
    if !valid_identifier(&config.id) || !valid_identifier(&config.family) || config.priority == 0 {
        return Err("engine id and family must be bounded identifiers".into());
    }
    validate_matcher(&config.matcher)?;
    if config.version.is_empty()
        || config.image_repository.is_empty()
        || !valid_digest(&config.image_digest)
        || !valid_identifier(&config.image_architecture)
    {
        return Err("engine image identity is invalid".into());
    }
    if config.entrypoint.is_empty()
        || config.entrypoint.iter().any(|value| !valid_argument(value))
        || config.arguments.iter().any(|value| !valid_argument(value))
        || config
            .environment
            .iter()
            .any(|value| !valid_environment(value))
    {
        return Err("engine command contains an invalid argument".into());
    }
    validate_executable_cache_environment(config)?;
    validate_placeholders(
        config.entrypoint.iter().chain(&config.arguments),
        &["{model_snapshot}", "{served_model}", "{port}"],
    )?;
    validate_artifact_arguments(&config.artifact_arguments)?;
    if !valid_managed_root(&config.model_cache_root)
        || !valid_managed_root(&config.compile_cache_root)
        || config.model_cache_root == config.compile_cache_root
        || !valid_identifier(&config.network)
        || matches!(config.network.as_str(), "bridge" | "host" | "none")
        || config.port == 0
        || config.run_as_uid == 0
        || config.pid_limit == 0
        || !valid_identifier(&config.seccomp)
        || config.seccomp == "unconfined"
        || !matches!(config.ipc_mode.as_str(), "private" | "host")
        || !(64 * 1024 * 1024..=32 * 1024 * 1024 * 1024).contains(&config.shm_size_bytes)
        || config.tmpfs != ["/tmp"]
        || !(1..=MAX_ENGINE_STARTUP_DEADLINE_SECONDS).contains(&config.startup_deadline_seconds)
    {
        return Err("engine isolation policy is invalid".into());
    }
    if !valid_resources(&config.resources)
        || config.semantic_prompt.trim().is_empty()
        || config.semantic_max_tokens == 0
        || config.health_method != "GET"
        || !valid_route(&config.health_path)
    {
        return Err("engine resource or health policy is invalid".into());
    }
    if config.health_body.as_ref().is_some_and(|rule| {
        rule.json_pointer.is_empty()
            || !rule.json_pointer.starts_with('/')
            || rule.json_pointer.len() > 128
            || rule.equals.is_empty()
            || rule.equals.len() > 128
    }) {
        return Err("engine health body predicate is invalid".into());
    }
    if config.routes.is_empty()
        || config.routes.iter().any(|route| {
            !matches!(route.method.as_str(), "GET" | "POST") || !valid_route(&route.path)
        })
    {
        return Err("engine gateway route policy is invalid".into());
    }
    validate_profiles(config)
}

fn validate_matcher(matcher: &EngineMatcher) -> Result<(), String> {
    let quantizations = matcher.quantizations.iter().collect::<BTreeSet<_>>();
    let profiles = matcher.engine_profiles.iter().collect::<BTreeSet<_>>();
    let capabilities = matcher
        .required_capabilities
        .iter()
        .collect::<BTreeSet<_>>();
    if matcher.formats.is_empty()
        || matcher
            .formats
            .iter()
            .enumerate()
            .any(|(index, format)| matcher.formats[..index].contains(format))
        || quantizations.len() != matcher.quantizations.len()
        || profiles.len() != matcher.engine_profiles.len()
        || profiles.iter().any(|profile| !valid_identifier(profile))
        || capabilities.len() != matcher.required_capabilities.len()
        || matcher
            .quantizations
            .iter()
            .any(|value| !valid_identifier(value))
        || matcher
            .required_capabilities
            .iter()
            .any(|value| !valid_identifier(value))
    {
        Err("engine artifact matcher is invalid".into())
    } else {
        Ok(())
    }
}

fn validate_artifact_arguments(arguments: &EngineArtifactArguments) -> Result<(), String> {
    validate_placeholders(arguments.primary.iter(), &["{model_file}"])?;
    let primary_count = arguments
        .primary
        .iter()
        .filter(|value| value.as_str() == "{model_file}")
        .count();
    if !arguments.primary.is_empty() && primary_count != 1 {
        Err("engine artifact arguments require one confined file placeholder".into())
    } else {
        let mut roles = BTreeSet::new();
        for binding in &arguments.bindings {
            validate_placeholders(binding.arguments.iter(), &["{auxiliary_file}"])?;
            let count = binding
                .arguments
                .iter()
                .filter(|value| value.as_str() == "{auxiliary_file}")
                .count();
            if !roles.insert(binding.role.clone()) || count != 1 {
                return Err("engine artifact role binding is invalid or ambiguous".into());
            }
        }
        if arguments
            .ignored_roles
            .iter()
            .any(|role| roles.contains(role))
            || arguments
                .ignored_roles
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != arguments.ignored_roles.len()
        {
            return Err("engine artifact role cannot be bound and ignored".into());
        }
        Ok(())
    }
}

fn expand_file_arguments(arguments: &[String], placeholder: &str, path: &str) -> Vec<String> {
    arguments
        .iter()
        .map(|argument| {
            if argument == placeholder {
                path.into()
            } else {
                argument.clone()
            }
        })
        .collect()
}

fn validate_placeholders<'a>(
    values: impl Iterator<Item = &'a String>,
    allowed: &[&str],
) -> Result<(), String> {
    for value in values {
        if !valid_argument(value)
            || ((value.contains('{') || value.contains('}'))
                && (!ALLOWED_PLACEHOLDERS.contains(&value.as_str())
                    || !allowed.contains(&value.as_str())))
        {
            return Err("engine command contains an unknown or embedded placeholder".into());
        }
    }
    Ok(())
}

fn validate_profiles(config: &EngineConfig) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    let mut model_types = BTreeSet::new();
    for profile in &config.profiles {
        let has_vision = profile
            .capabilities
            .iter()
            .any(|capability| capability == "vision");
        if !valid_identifier(&profile.id)
            || !ids.insert(&profile.id)
            || !(1_024..=1_048_576).contains(&profile.context_window)
            || profile.arguments.iter().any(|value| !valid_argument(value))
            || profile.capabilities.is_empty()
            || has_vision != profile.vision.is_some()
            || profile.capabilities.iter().any(|capability| {
                !matches!(
                    capability.as_str(),
                    "text_generation" | "text_embeddings" | "tool_calling" | "vision"
                )
            })
            || profile.embeddings.is_some()
                != profile
                    .capabilities
                    .iter()
                    .any(|value| value == "text_embeddings")
            || profile
                .model_types
                .iter()
                .any(|model_type| !valid_identifier(model_type) || !model_types.insert(model_type))
            || profile
                .resources
                .as_ref()
                .is_some_and(|resources| !valid_resources(resources))
            || profile.stream_idle_timeout_seconds > 3_600
            || if profile.native_responses {
                !(1..=3_600).contains(&profile.native_response_timeout_seconds)
            } else {
                profile.native_response_timeout_seconds != 0
            }
        {
            return Err("engine profile is invalid or ambiguous".into());
        }
        if let Some(vision) = &profile.vision {
            let processor = format!("sha256:{}", vision.processor_sha256);
            if !valid_digest(&processor) || super::gateway::vision_health_image(vision).is_err() {
                return Err("engine vision policy is invalid".into());
            }
        }
        if let Some(embedding) = &profile.embeddings {
            if !embedding.dimensions_from_model_config
                || embedding.max_batch == 0
                || embedding.max_input_bytes == 0
                || !embedding.normalized
                || embedding.normalization_tolerance_ppm == 0
            {
                return Err("engine embeddings policy is invalid".into());
            }
        }
        validate_sampling(&profile.sampling)?;
    }
    ids.contains(&config.default_profile)
        .then_some(())
        .ok_or_else(|| "engine default profile does not exist".into())
}

fn validate_sampling(sampling: &EngineSampling) -> Result<(), String> {
    let bounded = |value: Option<f64>, minimum: f64, maximum: f64| {
        value.is_none_or(|value| value.is_finite() && (minimum..=maximum).contains(&value))
    };
    let effort_map_valid = sampling
        .reasoning_effort_map
        .iter()
        .all(|(source, target)| {
            matches!(source.as_str(), "low" | "medium" | "high" | "max") && valid_identifier(target)
        });
    let default_effort_valid = sampling
        .default_reasoning_effort
        .as_ref()
        .is_none_or(|effort| sampling.reasoning_effort_map.contains_key(effort));
    let template_defaults_valid = sampling
        .chat_template_kwargs
        .keys()
        .all(|key| valid_identifier(key))
        && serde_json::to_vec(&sampling.chat_template_kwargs)
            .is_ok_and(|encoded| encoded.len() <= 4096);
    if bounded(sampling.temperature, 0.0, 2.0)
        && bounded(sampling.top_p, 0.0, 1.0)
        && bounded(sampling.min_p, 0.0, 1.0)
        && bounded(sampling.presence_penalty, -2.0, 2.0)
        && bounded(sampling.repetition_penalty, 0.0, 2.0)
        && sampling.top_k.is_none_or(|value| value > 0)
        && sampling
            .thinking_token_budget
            .is_none_or(|value| value > 0 && value <= super::gateway::MAX_OUTPUT_TOKENS)
        && effort_map_valid
        && default_effort_valid
        && template_defaults_valid
    {
        Ok(())
    } else {
        Err("engine profile sampling policy is invalid".into())
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_managed_root(value: &str) -> bool {
    let path = Path::new(value);
    path.is_absolute()
        && path.starts_with("/var/lib/sy-spark")
        && path != Path::new("/var/lib/sy-spark")
        && !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn valid_argument(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value
            .bytes()
            .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
}

fn valid_environment(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, value)| {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && valid_argument(value)
    })
}

fn validate_executable_cache_environment(config: &EngineConfig) -> Result<(), String> {
    let environment = config
        .environment
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .collect::<BTreeMap<_, _>>();
    let mut declared = BTreeSet::new();
    for name in &config.executable_cache_environment {
        let value = environment.get(name.as_str()).copied().unwrap_or_default();
        let path = Path::new(value);
        if !declared.insert(name)
            || !valid_environment(&format!("{name}=x"))
            || !path.is_absolute()
            || !path.starts_with("/compile-cache")
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err("executable cache path must be under /compile-cache".into());
        }
    }
    Ok(())
}

fn valid_route(value: &str) -> bool {
    value.starts_with('/')
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_engine_declaration_parses() {
        let directory = Path::new("configs/sy/spark/engines");
        for entry in std::fs::read_dir(directory).unwrap().filter_map(Result::ok) {
            if entry.path().extension().and_then(|value| value.to_str()) != Some("toml") {
                continue;
            }
            let text = std::fs::read_to_string(entry.path()).unwrap();
            EnginePolicy::parse(&text).unwrap();
        }
    }

    #[test]
    fn declared_executable_caches_must_avoid_noexec_tmpfs() {
        let path = std::fs::read_dir("configs/sy/spark/engines")
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|value| value.to_str()) == Some("toml"))
            .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let invalid = text.replacen(
            "environment = [",
            "executable_cache_environment = [\"JIT_CACHE_DIR\"]\nenvironment = [\n  \"JIT_CACHE_DIR=/tmp/jit\",",
            1,
        );
        let error = EnginePolicy::parse(&invalid).err().unwrap();
        assert!(error.contains("executable cache path must be under /compile-cache"));
    }

    #[test]
    fn isolated_compile_cache_root_can_host_executable_temporaries() {
        let text = std::fs::read_dir("configs/sy/spark/engines")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .find(|text| {
                EnginePolicy::parse(text)
                    .is_ok_and(|policy| policy.config().executable_cache_environment.is_empty())
            })
            .unwrap();
        let valid = text.replacen(
            "environment = [",
            "executable_cache_environment = [\"EXECUTABLE_TEMP_DIR\"]\nenvironment = [\n  \"EXECUTABLE_TEMP_DIR=/compile-cache\",",
            1,
        );
        EnginePolicy::parse(&valid).unwrap();
    }

    const LLAMA: &str = include_str!("../../configs/sy/spark/engines/llama-cpp.toml");
    const VLLM: &str = include_str!("../../configs/sy/spark/engines/vllm.toml");
    const VLLM_QWEN38: &str = include_str!("../../configs/sy/spark/engines/vllm-qwen38-mmap.toml");
    const VLLM_QWEN38_DOCKERFILE: &str =
        include_str!("../../configs/sy/spark/engines/vllm-qwen38-mmap.Dockerfile");

    fn catalog() -> EngineCatalog {
        EngineCatalog::parse_files([
            ("llama-cpp.toml", LLAMA),
            ("vllm.toml", VLLM),
            ("vllm-qwen38-mmap.toml", VLLM_QWEN38),
        ])
        .unwrap()
    }

    fn artifacts(format: ModelArtifactFormat, quantization: &str) -> ModelArtifactsDocument {
        ModelArtifactsDocument {
            schema: "sy.spark.model-artifacts/v2".into(),
            format,
            primary: super::super::wire::ModelArtifactFileDocument {
                path: "model.gguf".into(),
                bytes: 1,
                sha256: None,
            },
            auxiliary: Vec::new(),
            quantization: Some(quantization.into()),
            capabilities: vec!["text_generation".into()],
            configured_alias: None,
            engine_profile: None,
        }
    }

    #[test]
    fn gguf_selects_llama_and_safetensors_selects_vllm_from_config() {
        let catalog = catalog();
        let gguf = catalog
            .select(&artifacts(ModelArtifactFormat::Gguf, "Q4_K_XL"))
            .unwrap();
        let fp8 = catalog
            .select(&artifacts(ModelArtifactFormat::Safetensors, "FP8"))
            .unwrap();
        assert_eq!(
            (gguf.config().family.as_str(), fp8.config().family.as_str()),
            ("llama.cpp", "vllm")
        );
    }

    #[test]
    fn llama_blackwell_disables_cuda_graph_capture() {
        let policy = EnginePolicy::parse(LLAMA).unwrap();
        assert!(policy
            .config
            .environment
            .iter()
            .any(|value| value == "GGML_CUDA_DISABLE_GRAPHS=1"));
    }

    #[test]
    fn local_image_transport_selects_the_content_id() {
        let local = LLAMA.replacen(
            "image_transport = \"registry\"",
            "image_transport = \"local\"",
            1,
        );
        let policy = EnginePolicy::parse(&local).unwrap();

        assert_eq!(policy.image(), policy.config().image_digest);
    }

    #[test]
    fn equal_priority_match_is_rejected_instead_of_guessed() {
        let duplicate = LLAMA.replace("id = \"llama-cpp-cuda13-arm64\"", "id = \"second-llama\"");
        let catalog =
            EngineCatalog::parse_files([("first.toml", LLAMA), ("second.toml", &duplicate)])
                .unwrap();
        assert!(catalog
            .select(&artifacts(ModelArtifactFormat::Gguf, "Q4_K_XL"))
            .is_err());
    }

    #[test]
    fn engine_matcher_can_require_a_declarative_profile() {
        let specialized = VLLM
            .replace("id = \"vllm-arm64\"", "id = \"specialized\"")
            .replace("priority = 100", "priority = 200")
            .replace(
                "formats = [\"safetensors\"]",
                "formats = [\"safetensors\"]\nengine_profiles = [\"specialized-profile\"]",
            );
        let catalog = EngineCatalog::parse_files([
            ("generic.toml", VLLM),
            ("specialized.toml", specialized.as_str()),
        ])
        .unwrap();
        let mut configured = artifacts(ModelArtifactFormat::Safetensors, "NVFP4");

        assert_eq!(
            catalog.select(&configured).unwrap().config().id,
            "vllm-arm64"
        );
        configured.engine_profile = Some("specialized-profile".into());
        assert_eq!(
            catalog.select(&configured).unwrap().config().id,
            "specialized"
        );
    }

    #[test]
    fn shipped_specialized_engine_requires_its_configured_profile() {
        let mut configured = artifacts(ModelArtifactFormat::Safetensors, "NVFP4");
        assert_eq!(
            catalog().select(&configured).unwrap().config().id,
            "vllm-arm64"
        );

        configured.engine_profile = Some("qwen3.8-flash-next-nvfp4".into());
        let engines = catalog();
        let selected = engines.select(&configured).unwrap();
        assert_eq!(selected.config().id, "vllm-qwen38-mmap-arm64");
        assert_eq!(selected.config().startup_deadline_seconds, 1_800);
        assert_eq!(selected.config().ipc_mode, "host");
        assert_eq!(selected.config().shm_size_bytes, 17_179_869_184);
        let arguments = &selected.config().profiles[0].arguments;
        assert!(arguments
            .windows(2)
            .any(|pair| { pair == ["--kv-cache-memory", "12884901888"] }));
        assert!(!arguments
            .iter()
            .any(|arg| arg == "--gpu-memory-utilization"));
    }

    #[test]
    fn engine_profile_can_bound_reasoning_without_disabling_it() {
        let sampling = EnginePolicy::parse(VLLM_QWEN38)
            .unwrap()
            .gateway_profile(Some("qwen4_exp"))
            .sampling;

        assert_eq!(sampling.defaults["thinking_token_budget"], 8192.into());
        assert_eq!(
            sampling.reasoning_effort_map,
            BTreeMap::from([
                ("low".into(), "low".into()),
                ("medium".into(), "medium".into()),
                ("high".into(), "xhigh".into()),
                ("max".into(), "xhigh".into()),
            ])
        );
    }

    #[test]
    fn engine_profile_accepts_declarative_reasoning_effort_map() {
        let config = VLLM_QWEN38.replace(
            "thinking_token_budget = 8192 }",
            "thinking_token_budget = 8192, reasoning_effort_map = { low = \"low\" } }",
        );

        let effort_map = EnginePolicy::parse(&config)
            .unwrap()
            .gateway_profile(Some("qwen4_exp"))
            .sampling
            .reasoning_effort_map;

        assert_eq!(effort_map["low"], "low");
    }

    #[test]
    fn engine_profile_rejects_unknown_reasoning_effort_map_keys() {
        let config = VLLM_QWEN38.replace("low = \"low\"", "turbo = \"low\"");

        assert!(EnginePolicy::parse(&config).is_err());
    }

    #[test]
    fn recommended_nvfp4_model_selects_the_patched_engine_from_config() {
        let models = super::super::model_catalog::ModelCatalog::parse(include_str!(
            "../../configs/sy/spark/models.toml"
        ))
        .unwrap();
        let model = models.resolve("qwen3.8:flash-next-nvfp4").unwrap();

        assert_eq!(
            catalog().select(model.artifacts()).unwrap().config().id,
            "vllm-qwen38-mmap-arm64"
        );
    }

    #[test]
    fn shipped_freetoken_engine_is_declarative_and_spark_tuned() {
        let text = std::fs::read_to_string("configs/sy/spark/engines/freetoken.toml").unwrap();
        let policy = EnginePolicy::parse(&text).unwrap();
        let profile = policy
            .profile_for(None, &artifacts(ModelArtifactFormat::Safetensors, "NVFP4"))
            .unwrap();

        assert_eq!(policy.config().family, "freetoken");
        assert_eq!(profile.context_window, 262_144);
        assert!(profile.native_responses);
        assert_eq!(profile.native_response_timeout_seconds, 1_800);
        assert_eq!(profile.sampling.default_reasoning_effort, None);
        assert_eq!(
            profile.sampling.chat_template_kwargs["preserve_thinking"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(
            (
                profile.sampling.temperature,
                profile.sampling.top_p,
                profile.sampling.top_k,
                profile.sampling.min_p,
                profile.sampling.presence_penalty,
                profile.sampling.repetition_penalty,
            ),
            (
                Some(0.6),
                Some(0.95),
                Some(20),
                Some(0.0),
                Some(0.0),
                Some(1.0)
            )
        );
        assert!(profile
            .arguments
            .windows(2)
            .any(|pair| pair == ["--moe-backend", "auto"]));
        assert_eq!(
            policy.config().health_body.as_ref().unwrap(),
            &EngineHealthBody {
                json_pointer: "/status".into(),
                equals: "ok".into(),
            }
        );
        assert!(policy
            .config()
            .environment
            .contains(&"FLASHINFER_WORKSPACE_BASE=/compile-cache/flashinfer".into()));
        assert!(policy
            .config()
            .environment
            .contains(&"TVM_FFI_CACHE_DIR=/compile-cache/tvm-ffi".into()));
        assert!(profile
            .arguments
            .windows(2)
            .any(|pair| pair == ["--num-tokens", "262144"]));
    }

    #[test]
    fn shipped_vllm_profile_owns_long_prefill_stream_timeout() {
        let policy = EnginePolicy::parse(include_str!(
            "../../configs/sy/spark/engines/vllm-qwen38-mmap.toml"
        ))
        .unwrap();
        let profile = policy.gateway_profile(None);
        assert_eq!(profile.stream_idle_timeout_seconds, 600);
        assert_eq!(
            profile.stream_idle_timeout(),
            std::time::Duration::from_secs(600)
        );
    }

    #[test]
    fn stream_idle_timeout_override_is_bounded() {
        let config = include_str!("../../configs/sy/spark/engines/vllm-qwen38-mmap.toml").replace(
            "stream_idle_timeout_seconds = 600",
            "stream_idle_timeout_seconds = 3601",
        );
        assert!(EnginePolicy::parse(&config).is_err());
    }

    #[test]
    fn patched_vllm_module_is_readable_by_non_root_runtime() {
        assert!(VLLM_QWEN38_DOCKERFILE.contains("chmod 0644 \"${SITE_PACKAGES}/vllm_ple_mmap.py\""));
    }

    #[test]
    fn artifact_placeholders_are_confined_and_exact() {
        let embedded = LLAMA.replacen("\"{model_file}\"", "\"/models/{model_file}\"", 1);
        let unknown = LLAMA.replacen("\"{model_file}\"", "\"{checkpoint}\"", 1);
        assert!(EnginePolicy::parse(&embedded).is_err());
        assert!(EnginePolicy::parse(&unknown).is_err());
    }

    #[test]
    fn engine_emits_only_arguments_bound_to_the_auxiliary_role() {
        let policy = EnginePolicy::parse(LLAMA).unwrap();
        let mut artifacts = artifacts(ModelArtifactFormat::Gguf, "Q4_K_XL");
        artifacts.auxiliary = vec![
            super::super::wire::ModelAuxiliaryArtifactDocument {
                role: ModelArtifactRole::Projector,
                path: "mmproj.gguf".into(),
                bytes: 1,
                sha256: None,
            },
            super::super::wire::ModelAuxiliaryArtifactDocument {
                role: ModelArtifactRole::WeightShard,
                path: "model-00002-of-00002.gguf".into(),
                bytes: 1,
                sha256: None,
            },
            super::super::wire::ModelAuxiliaryArtifactDocument {
                role: "draft_model".parse().unwrap(),
                path: "mtp.gguf".into(),
                bytes: 1,
                sha256: None,
            },
        ];

        let argv = policy
            .artifact_argv(&artifacts, |path| {
                Ok(format!("/models/snapshots/commit/{path}"))
            })
            .unwrap();

        assert_eq!(
            argv,
            [
                "--model",
                "/models/snapshots/commit/model.gguf",
                "--mmproj",
                "/models/snapshots/commit/mmproj.gguf",
                "--model-draft",
                "/models/snapshots/commit/mtp.gguf",
            ]
        );
        assert!(!argv.iter().any(|value| value.contains("00002")));
    }

    #[test]
    fn duplicate_or_missing_projector_role_bindings_fail_closed() {
        let duplicate = LLAMA.replace(
            "[[artifact_arguments.bindings]]",
            "[[artifact_arguments.bindings]]\nrole = \"projector\"\narguments = [\"--other\", \"{auxiliary_file}\"]\n\n[[artifact_arguments.bindings]]",
        );
        assert!(EnginePolicy::parse(&duplicate).is_err());

        let policy = EnginePolicy::parse(VLLM).unwrap();
        let mut artifacts = artifacts(ModelArtifactFormat::Safetensors, "FP8");
        artifacts.auxiliary = vec![super::super::wire::ModelAuxiliaryArtifactDocument {
            role: ModelArtifactRole::Projector,
            path: "mmproj.safetensors".into(),
            bytes: 1,
            sha256: None,
        }];
        assert!(policy
            .artifact_argv(&artifacts, |path| {
                Ok(format!("/models/snapshots/commit/{path}"))
            })
            .is_err());

        artifacts.auxiliary[0].role = "draft_model".parse().unwrap();
        assert!(policy
            .artifact_argv(&artifacts, |_| Ok("draft.gguf".into()))
            .is_err());
    }

    #[test]
    fn duplicate_ids_and_unsupported_formats_fail_at_catalog_load() {
        let duplicate = EngineCatalog::parse_files([("one.toml", VLLM), ("two.toml", VLLM)]);
        let unsupported = LLAMA.replacen("formats = [\"gguf\"]", "formats = [\"onnx\"]", 1);
        assert!(duplicate.is_err());
        assert!(EnginePolicy::parse(&unsupported).is_err());
    }

    #[test]
    fn operational_values_live_only_in_configuration() {
        let policy = EnginePolicy::parse(VLLM).unwrap();
        let mut values = vec![
            policy.config.id.as_str(),
            policy.config.version.as_str(),
            policy.config.image_repository.as_str(),
            policy.config.image_digest.as_str(),
            policy.config.model_cache_root.as_str(),
            policy.config.compile_cache_root.as_str(),
            policy.config.network.as_str(),
        ];
        values.extend(policy.config.entrypoint.iter().map(String::as_str));
        values.extend(policy.config.arguments.iter().map(String::as_str));
        values.extend(policy.config.environment.iter().map(String::as_str));
        values.extend(policy.config.profiles.iter().flat_map(|profile| {
            profile
                .model_types
                .iter()
                .map(String::as_str)
                .chain(profile.arguments.iter().map(String::as_str))
        }));
        let runtime = ["engine.rs", "executor.rs", "gateway.rs", "agent.rs"]
            .into_iter()
            .map(|name| {
                let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/spark")
                    .join(name);
                let source = std::fs::read_to_string(path).unwrap();
                source
                    .split("\n#[cfg(test)]\nmod tests")
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for value in values
            .into_iter()
            .filter(|value| value.len() >= 7 && !value.contains('{'))
        {
            assert!(!runtime.contains(value), "runtime source embeds {value}");
        }
        for value in [
            policy.config.resources.image_bytes,
            policy.config.resources.startup_peak_bytes,
            policy.config.resources.steady_peak_bytes,
            policy.config.resources.compile_cache_bytes,
        ] {
            assert!(
                !runtime.contains(&value.to_string()),
                "runtime source embeds resource value {value}"
            );
        }
        for forbidden in ["ORNITH_", "QWEN_", "0.6", "0.95"] {
            assert!(
                !runtime.contains(forbidden),
                "runtime source embeds model policy {forbidden}"
            );
        }
    }

    #[test]
    fn config_selects_profiles_from_model_metadata() {
        let policy = EnginePolicy::parse(VLLM).unwrap();
        assert_eq!(policy.profile(Some("qwen3_5")).id, "qwen3_5");
        assert_eq!(policy.profile(Some("llama")).id, "text");
    }

    #[test]
    fn profile_can_publish_declarative_vision_policy() {
        let configured = LLAMA.replace(
            "capabilities = [\"text_generation\", \"tool_calling\"]",
            "capabilities = [\"text_generation\", \"tool_calling\", \"vision\"]\nvision = { processor_sha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\", media_types = [\"image/png\"], max_bytes = 1024, max_total_bytes = 1024, max_count = 1, max_width = 16, max_height = 16, health_media_type = \"image/png\", health_image_base64 = \"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=\", health_image_sha256 = \"431ced6916a2a21a156e38701afe55bbd7f88969fbbfc56d7fe099d47f265460\", health_prompt = \"Name the color.\", health_expected_text = \"black\", health_max_tokens = 8, health_disable_thinking = true }",
        );
        assert!(EnginePolicy::parse(&configured)
            .unwrap()
            .gateway_profile(None)
            .vision
            .is_some());
    }

    #[test]
    fn vision_capability_requires_its_policy() {
        let configured = LLAMA
            .lines()
            .filter(|line| !line.starts_with("vision = "))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(EnginePolicy::parse(&configured).is_err());
    }

    #[test]
    fn shipped_llama_profile_declares_vision() {
        let profile = EnginePolicy::parse(LLAMA).unwrap().gateway_profile(None);
        assert!(profile.capabilities.contains("vision"));
        assert!(profile.vision.is_some());
    }

    #[test]
    fn text_only_artifact_does_not_publish_engine_vision() {
        let artifact = artifacts(ModelArtifactFormat::Gguf, "Q4_K_M");
        let profile = EnginePolicy::parse(LLAMA)
            .unwrap()
            .gateway_profile_for(None, &artifact, None)
            .unwrap();
        assert_eq!(
            profile.capabilities,
            ["text_generation".into(), "tool_calling".into()].into()
        );
        assert!(profile.vision.is_none());
    }

    #[test]
    fn shipped_vision_probe_has_reasoning_budget() {
        let profile = EnginePolicy::parse(LLAMA).unwrap().gateway_profile(None);
        assert!(profile.vision.unwrap().health_max_tokens >= 256);
    }

    #[test]
    fn shipped_vision_fixture_fits_encoder_resolution() {
        let profile = EnginePolicy::parse(LLAMA).unwrap().gateway_profile(None);
        let image = super::super::gateway::vision_health_image(&profile.vision.unwrap()).unwrap();
        assert!(image.width >= 224 && image.height >= 224);
    }

    #[test]
    fn llama_context_is_loaded_from_each_model_instead_of_globally_capped() {
        let policy = EnginePolicy::parse(LLAMA).unwrap();
        assert!(policy
            .config
            .arguments
            .windows(2)
            .any(|pair| pair == ["--ctx-size", "0"]));
    }

    #[test]
    fn llama_engine_does_not_impose_global_reasoning_policy() {
        let policy = EnginePolicy::parse(LLAMA).unwrap();
        let profile = policy.profile(None);
        assert!(!profile
            .arguments
            .windows(2)
            .any(|pair| pair == ["--reasoning", "off"]));
        assert!(!profile
            .arguments
            .iter()
            .any(|arg| arg == "--reasoning-budget"));
        assert!(profile
            .arguments
            .iter()
            .any(|arg| arg == "--reasoning-preserve"));
    }

    #[test]
    fn artifact_selects_a_declarative_reasoning_profile() {
        let policy = EnginePolicy::parse(LLAMA).unwrap();
        let mut configured = artifacts(ModelArtifactFormat::Gguf, "Q4_K_M");
        configured.engine_profile = Some("ornith-coding".into());
        let profile = policy.profile_for(None, &configured).unwrap();
        assert_eq!(profile.id, "ornith-coding");
        assert!(profile
            .arguments
            .windows(2)
            .any(|pair| pair == ["--reasoning-budget", "16384"]));
        assert!(policy
            .profile_for(None, &artifacts(ModelArtifactFormat::Gguf, "Q4_K_M"))
            .unwrap()
            .arguments
            .iter()
            .all(|argument| argument != "--reasoning-budget"));
    }

    #[test]
    fn qwen_mtp_profile_preserves_unbounded_reasoning() {
        let policy = EnginePolicy::parse(LLAMA).unwrap();
        let mut configured = artifacts(ModelArtifactFormat::Gguf, "Q4_K_M");
        configured.engine_profile = Some("qwen3.8-mtp".into());
        let arguments = &policy.profile_for(None, &configured).unwrap().arguments;
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["--spec-type", "draft-mtp"]));
        assert!(arguments
            .iter()
            .any(|argument| argument == "--reasoning-preserve"));
        assert!(!arguments
            .iter()
            .any(|argument| argument == "--reasoning-budget"));
    }

    #[test]
    fn qwen_mtp_profile_disables_stateful_startup_protocol_probe() {
        let policy = EnginePolicy::parse(LLAMA).unwrap();
        let mut configured = artifacts(ModelArtifactFormat::Gguf, "Q4_K_M");
        configured.engine_profile = Some("qwen3.8-mtp".into());
        assert!(
            !policy
                .gateway_profile_for(None, &configured, None)
                .unwrap()
                .startup_protocol_probe
        );
    }

    #[test]
    fn native_response_timeout_is_required_and_bounded() {
        let config = include_str!("../../configs/sy/spark/engines/freetoken.toml");
        for invalid in [0, 3_601] {
            let config = config.replace(
                "native_response_timeout_seconds = 1800",
                &format!("native_response_timeout_seconds = {invalid}"),
            );
            assert!(EnginePolicy::parse(&config).is_err());
        }
    }

    #[test]
    fn declarative_profile_overrides_engine_resources() {
        let configured_policy = LLAMA.replacen(
            "id = \"qwen3.8-mtp\"",
            "id = \"qwen3.8-mtp\"\nresources = { image_bytes = 6, startup_peak_bytes = 7, steady_peak_bytes = 8, compile_cache_bytes = 9 }",
            1,
        );
        let policy = EnginePolicy::parse(&configured_policy).unwrap();
        let mut configured = artifacts(ModelArtifactFormat::Gguf, "Q4_K_M");
        configured.engine_profile = Some("qwen3.8-mtp".into());

        assert_eq!(
            policy.resources_for(None, &configured).unwrap(),
            RecipeResourceEnvelopeDocument {
                image_bytes: 6,
                startup_peak_bytes: 7,
                steady_peak_bytes: 8,
                compile_cache_bytes: 9,
            }
        );
        assert_eq!(
            policy
                .resources_for(None, &artifacts(ModelArtifactFormat::Gguf, "Q4_K_M"))
                .unwrap()
                .startup_peak_bytes,
            policy.config.resources.startup_peak_bytes
        );
    }

    #[test]
    fn unknown_declarative_profile_is_rejected() {
        let policy = EnginePolicy::parse(LLAMA).unwrap();
        let mut configured = artifacts(ModelArtifactFormat::Gguf, "Q4_K_M");
        configured.engine_profile = Some("missing".into());
        assert!(policy.profile_for(None, &configured).is_err());
    }

    #[test]
    fn llama_profile_uses_precise_coding_sampling() {
        let sampling = EnginePolicy::parse(LLAMA)
            .unwrap()
            .gateway_profile(None)
            .sampling;
        assert_eq!(
            sampling.defaults,
            sampling_policy(&EngineSampling {
                temperature: Some(0.6),
                top_p: Some(0.95),
                top_k: Some(20),
                min_p: Some(0.0),
                presence_penalty: Some(0.0),
                repetition_penalty: Some(1.0),
                thinking_token_budget: None,
                default_reasoning_effort: None,
                reasoning_effort_map: BTreeMap::new(),
                chat_template_kwargs: BTreeMap::new(),
            })
            .defaults
        );
    }

    #[test]
    fn invalid_vision_fixture_is_rejected() {
        let changed = LLAMA.replace(
            "1b8ab2918e9fb346d8ff5b7579372510b0d8b1566d590dac31aea311fff15fc6",
            &"a".repeat(64),
        );
        assert!(EnginePolicy::parse(&changed).is_err());
    }

    #[test]
    fn qwen35_profile_serializes_mixed_gdn_batches() {
        let policy = EnginePolicy::parse(VLLM).unwrap();
        let profile = policy.profile(Some("qwen3_5"));
        assert!(profile
            .arguments
            .windows(2)
            .any(|pair| pair == ["--max-num-seqs", "1"]));
    }

    #[test]
    fn qwen3_profile_preserves_parallel_serving() {
        let policy = EnginePolicy::parse(VLLM).unwrap();
        let profile = policy.profile(Some("qwen3"));
        assert!(profile
            .arguments
            .windows(2)
            .any(|pair| pair == ["--max-num-seqs", "16"]));
    }

    #[test]
    fn config_rejects_unbounded_profile_arguments() {
        let invalid = VLLM.replacen(
            "arguments = [\"--max-num-seqs\", \"16\"]",
            "arguments = [\"line\\nbreak\"]",
            1,
        );
        assert!(EnginePolicy::parse(&invalid).is_err());
    }

    #[test]
    fn safetensors_defaults_to_vllm_without_a_quantization_recipe() {
        let mut generic = artifacts(ModelArtifactFormat::Safetensors, "BF16");
        generic.quantization = None;
        assert_eq!(catalog().select(&generic).unwrap().config().family, "vllm");
    }

    #[test]
    fn vllm_embedding_profile_uses_model_config_dimensions() {
        let mut embedding = artifacts(ModelArtifactFormat::Safetensors, "BF16");
        embedding.capabilities = vec!["text_embeddings".into()];
        let profile = EnginePolicy::parse(VLLM)
            .unwrap()
            .gateway_profile_for(None, &embedding, Some(1024))
            .unwrap();
        assert_eq!(profile.embeddings.unwrap().dimensions, 1024);
    }
}
