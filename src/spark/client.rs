//! Pinned-CA HTTPS client for normal Spark reads after SSH bootstrap.

use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use reqwest::{
    blocking::{Client, Response},
    Method, StatusCode, Url,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::install::BootstrapMaterial;
use super::{
    wire::{
        DownloadPlanDocument, DownloadRequest, EngineLogDocument, InstanceListDocument,
        ModelDocument, ModelListDocument, OperationDocument, OperationEvent, OperationListDocument,
        ProblemDocument, RemovalPlanDocument, RemoveModelRequest, ServeAdmissionRequest,
        ServeRequest, StopRequest, TokenCreateRequest, TokenCreatedDocument, TokenListDocument,
    },
    EXIT_INTERNAL, EXIT_OPERATION_FAILED, EXIT_REJECTED, EXIT_UNREACHABLE, EXIT_USAGE,
};

const MAX_SAFE_READ_ATTEMPTS: usize = 3;
const OPERATION_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profiles {
    hosts: std::collections::BTreeMap<String, HostProfile>,
}

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProfile {
    pub url: String,
    pub ca_cert_sha256: String,
    pub credential: String,
    pub request_timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexClientConfig {
    pub schema: String,
    pub client: String,
    pub instance: String,
    pub model: String,
    pub base_url: String,
    pub env_key: String,
    pub wire_api: String,
    pub ca_env_key: String,
    pub ca_path: PathBuf,
    pub config_path: String,
    pub toml: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeCodeClientConfig {
    pub schema: String,
    pub client: String,
    pub pinned_version: String,
    pub instance: String,
    pub model: String,
    pub base_url: String,
    pub secret_env_key: String,
    pub ca_env_key: String,
    pub ca_path: PathBuf,
    pub config_path: String,
    pub shell: String,
}

pub fn codex_client_config(
    config_dir: &Path,
    host: &str,
    instance: &str,
    model: &str,
) -> Result<CodexClientConfig, ClientError> {
    validate_instance_reference(instance)?;
    let profiles: Profiles = toml::from_str(&read_text(&config_dir.join("spark.toml"))?)
        .map_err(|error| usage(format!("invalid Spark host profiles: {error}")))?;
    let profile = profiles
        .hosts
        .get(host)
        .ok_or_else(|| usage(format!("Spark host profile {host:?} is not configured")))?;
    let base =
        Url::parse(&profile.url).map_err(|error| usage(format!("invalid Spark URL: {error}")))?;
    reject_plaintext_lan(&base)?;
    let base_url = base
        .join(&format!("openai/{instance}/v1"))
        .map_err(|error| usage(format!("invalid inference route: {error}")))?
        .to_string()
        .trim_end_matches('/')
        .into();
    Ok(build_codex_config(
        config_dir, host, instance, model, base_url,
    ))
}

pub fn claude_code_client_config(
    config_dir: &Path,
    host: &str,
    instance: &str,
    model: &str,
) -> Result<ClaudeCodeClientConfig, ClientError> {
    validate_instance_reference(instance)?;
    let profiles: Profiles = toml::from_str(&read_text(&config_dir.join("spark.toml"))?)
        .map_err(|error| usage(format!("invalid Spark host profiles: {error}")))?;
    let profile = profiles
        .hosts
        .get(host)
        .ok_or_else(|| usage(format!("Spark host profile {host:?} is not configured")))?;
    let base =
        Url::parse(&profile.url).map_err(|error| usage(format!("invalid Spark URL: {error}")))?;
    reject_plaintext_lan(&base)?;
    let base_url = base
        .join(&format!("anthropic/{instance}"))
        .map_err(|error| usage(format!("invalid inference route: {error}")))?
        .to_string()
        .trim_end_matches('/')
        .to_owned();
    Ok(build_claude_code_config(
        config_dir, host, instance, model, base_url,
    ))
}

fn build_claude_code_config(
    config_dir: &Path,
    host: &str,
    instance: &str,
    model: &str,
    base_url: String,
) -> ClaudeCodeClientConfig {
    let ca_path = config_dir.join("spark").join(format!("{host}.ca.pem"));
    let shell = format!(
        "export ANTHROPIC_BASE_URL={}\nexport ANTHROPIC_MODEL={}\nexport ANTHROPIC_SMALL_FAST_MODEL={}\nexport CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1\nexport CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1\nexport CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1\nexport NODE_EXTRA_CA_CERTS={}\n",
        shell_quote(&base_url), shell_quote(model), shell_quote(model),
        shell_quote(&ca_path.to_string_lossy())
    );
    ClaudeCodeClientConfig {
        schema: "sy.spark.client-config/v1".into(),
        client: "claude-code".into(),
        pinned_version: "2.1.241".into(),
        instance: instance.into(),
        model: model.into(),
        base_url,
        secret_env_key: "ANTHROPIC_API_KEY".into(),
        ca_env_key: "NODE_EXTRA_CA_CERTS".into(),
        ca_path,
        config_path: "user shell environment; no project settings file".into(),
        shell,
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn build_codex_config(
    config_dir: &Path,
    host: &str,
    instance: &str,
    model: &str,
    base_url: String,
) -> CodexClientConfig {
    const ENV_KEY: &str = "SY_SPARK_INFERENCE_TOKEN";
    let provider = format!("sy_spark_{}", instance.replace(['-', '.'], "_"));
    let toml = format!(
        "model = {model:?}\nmodel_provider = {provider:?}\nweb_search = \"disabled\"\n\n[model_providers.{provider}]\nname = {name:?}\nbase_url = {base_url:?}\nenv_key = {env:?}\nwire_api = \"responses\"\nsupports_standalone_web_search = false\nsupports_websockets = false\n",
        name = format!("sy Spark {instance}"),
        env = ENV_KEY
    );
    CodexClientConfig {
        schema: "sy.spark.client-config/v1".into(),
        client: "codex".into(),
        instance: instance.into(),
        model: model.into(),
        base_url,
        env_key: ENV_KEY.into(),
        wire_api: "responses".into(),
        ca_env_key: "SSL_CERT_FILE".into(),
        ca_path: config_dir.join("spark").join(format!("{host}.ca.pem")),
        config_path: "$CODEX_HOME/config.toml (defaults to ~/.codex/config.toml)".into(),
        toml,
    }
}

#[derive(Debug)]
pub struct ClientError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientError {}

pub struct SparkClient {
    http: Client,
    base: Url,
    token: String,
    request_timeout: Duration,
}

impl std::fmt::Debug for SparkClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SparkClient")
            .field("base", &self.base)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl Drop for SparkClient {
    fn drop(&mut self) {
        // Bearer material must not remain in reusable heap storage.
        self.token.replace_range(.., &"0".repeat(self.token.len()));
        self.token.clear();
    }
}

impl SparkClient {
    pub fn load(config_dir: &Path, host: &str) -> Result<Self, ClientError> {
        let profiles: Profiles = toml::from_str(&read_text(&config_dir.join("spark.toml"))?)
            .map_err(|error| usage(format!("invalid Spark host profiles: {error}")))?;
        let profile = profiles
            .hosts
            .get(host)
            .ok_or_else(|| usage(format!("Spark host profile {host:?} is not configured")))?;
        let base = Url::parse(&profile.url)
            .map_err(|error| usage(format!("invalid Spark URL: {error}")))?;
        reject_plaintext_lan(&base)?;
        let ca_path = config_dir.join("spark").join(format!("{host}.ca.pem"));
        let ca_pem = fs::read(&ca_path)
            .map_err(|_| unreachable("pinned Spark CA certificate is unavailable"))?;
        let actual_pin = format!("sha256:{:x}", Sha256::digest(&ca_pem));
        if actual_pin != profile.ca_cert_sha256 {
            return Err(unreachable("pinned Spark CA fingerprint does not match"));
        }
        let token_path = credential_path(config_dir, &profile.credential)?;
        enforce_private_mode(&token_path)?;
        let token = read_text(&token_path)?.trim().to_owned();
        if token.is_empty() {
            return Err(unreachable("Spark credential is empty"));
        }
        let certificate = reqwest::Certificate::from_pem(&ca_pem)
            .map_err(|_| unreachable("pinned Spark CA certificate is invalid"))?;
        let request_timeout = Duration::from_secs(profile.request_timeout_seconds);
        let http = Client::builder()
            .tls_built_in_root_certs(false)
            .add_root_certificate(certificate)
            .https_only(base.scheme() == "https")
            .timeout(request_timeout)
            .build()
            .map_err(|_| unreachable("could not construct pinned Spark HTTPS client"))?;
        Ok(Self {
            http,
            base,
            token,
            request_timeout,
        })
    }

    pub fn get_json<T: serde::de::DeserializeOwned>(&self, route: &str) -> Result<T, ClientError> {
        let url = self
            .base
            .join(route)
            .map_err(|error| usage(format!("invalid Spark route: {error}")))?;
        for attempt in 0..MAX_SAFE_READ_ATTEMPTS {
            match self.http.get(url.clone()).bearer_auth(&self.token).send() {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json()
                        .map_err(|_| internal("Spark returned an incompatible JSON document"));
                }
                Ok(response)
                    if response.status() == StatusCode::UNAUTHORIZED
                        || response.status() == StatusCode::FORBIDDEN =>
                {
                    return Err(unreachable("Spark authentication failed"));
                }
                Ok(response) => return Err(map_problem(response.status(), response.json().ok())),
                Err(_) if attempt + 1 < MAX_SAFE_READ_ATTEMPTS => {
                    thread::sleep(Duration::from_millis(50 * (attempt as u64 + 1)));
                }
                Err(_) => {
                    return Err(unreachable(
                        "Spark agent is unreachable or its TLS identity is invalid",
                    ));
                }
            }
        }
        Err(unreachable("Spark agent is unreachable"))
    }

    pub fn list_operations(&self) -> Result<OperationListDocument, ClientError> {
        self.get_json("api/sy.spark/v1/operations")
    }

    pub fn list_models(&self) -> Result<ModelListDocument, ClientError> {
        self.get_json("api/sy.spark/v1/models")
    }

    pub fn model(&self, id: &str) -> Result<ModelDocument, ClientError> {
        if !valid_model_id(id) {
            return Err(usage("Spark model ID is invalid"));
        }
        self.get_json(&format!("api/sy.spark/v1/models/{id}"))
    }

    pub fn download_plan(
        &self,
        key: &str,
        request: &DownloadRequest,
    ) -> Result<DownloadPlanDocument, ClientError> {
        self.mutation_json(
            Method::POST,
            "api/sy.spark/v1/downloads",
            key,
            Some(request),
        )
    }

    pub fn download(
        &self,
        key: &str,
        request: &DownloadRequest,
    ) -> Result<OperationDocument, ClientError> {
        self.mutation_json(
            Method::POST,
            "api/sy.spark/v1/downloads",
            key,
            Some(request),
        )
    }

    pub fn admission_plan(
        &self,
        key: &str,
        request: &ServeAdmissionRequest,
    ) -> Result<super::resources::AdmissionReport, ClientError> {
        self.mutation_json(
            Method::POST,
            "api/sy.spark/v1/admission",
            key,
            Some(request),
        )
    }

    pub fn serve(
        &self,
        key: &str,
        request: &ServeRequest,
    ) -> Result<OperationDocument, ClientError> {
        self.mutation_json(
            Method::POST,
            "api/sy.spark/v1/instances",
            key,
            Some(request),
        )
    }

    pub fn instances(&self) -> Result<InstanceListDocument, ClientError> {
        self.get_json("api/sy.spark/v1/instances")
    }

    pub fn stop_instance(
        &self,
        id: &str,
        key: &str,
        request: &StopRequest,
    ) -> Result<OperationDocument, ClientError> {
        validate_instance_reference(id)?;
        self.mutation_json(
            Method::DELETE,
            &format!("api/sy.spark/v1/instances/{id}"),
            key,
            Some(request),
        )
    }

    pub fn stop_plan(
        &self,
        id: &str,
        key: &str,
        request: &StopRequest,
    ) -> Result<super::wire::InstanceDocument, ClientError> {
        validate_instance_reference(id)?;
        self.mutation_json(
            Method::DELETE,
            &format!("api/sy.spark/v1/instances/{id}"),
            key,
            Some(request),
        )
    }

    pub fn instance_logs(
        &self,
        id: &str,
        cursor: u64,
        limit: usize,
    ) -> Result<EngineLogDocument, ClientError> {
        validate_instance_reference(id)?;
        self.get_json(&format!(
            "api/sy.spark/v1/instances/{id}/logs?cursor={cursor}&limit={limit}"
        ))
    }

    pub fn removal_plan(
        &self,
        id: &str,
        key: &str,
        request: &RemoveModelRequest,
    ) -> Result<RemovalPlanDocument, ClientError> {
        if !valid_model_id(id) {
            return Err(usage("Spark model ID is invalid"));
        }
        self.mutation_json(
            Method::DELETE,
            &format!("api/sy.spark/v1/models/{id}"),
            key,
            Some(request),
        )
    }

    pub fn remove_model(
        &self,
        id: &str,
        key: &str,
        request: &RemoveModelRequest,
    ) -> Result<OperationDocument, ClientError> {
        if !valid_model_id(id) {
            return Err(usage("Spark model ID is invalid"));
        }
        self.mutation_json(
            Method::DELETE,
            &format!("api/sy.spark/v1/models/{id}"),
            key,
            Some(request),
        )
    }

    pub fn operation(&self, id: &str) -> Result<OperationDocument, ClientError> {
        validate_id(id)?;
        self.get_json(&format!("api/sy.spark/v1/operations/{id}"))
    }

    pub fn cancel_operation(&self, id: &str, key: &str) -> Result<OperationDocument, ClientError> {
        validate_id(id)?;
        self.mutation_json::<(), _>(
            Method::DELETE,
            &format!("api/sy.spark/v1/operations/{id}"),
            key,
            None,
        )
    }

    pub fn create_token(
        &self,
        key: &str,
        request: &TokenCreateRequest,
    ) -> Result<TokenCreatedDocument, ClientError> {
        self.mutation_json(Method::POST, "api/sy.spark/v1/tokens", key, Some(request))
    }

    pub fn list_tokens(&self) -> Result<TokenListDocument, ClientError> {
        self.get_json("api/sy.spark/v1/tokens")
    }

    pub fn revoke_token(&self, id: &str, key: &str) -> Result<OperationDocument, ClientError> {
        validate_id(id)?;
        self.mutation_json::<(), _>(
            Method::DELETE,
            &format!("api/sy.spark/v1/tokens/{id}"),
            key,
            None,
        )
    }

    pub fn follow_operation(
        &self,
        id: &str,
        last_event_id: u64,
    ) -> Result<OperationDocument, ClientError> {
        validate_id(id)?;
        let deadline = Instant::now()
            .checked_add(self.request_timeout)
            .ok_or_else(|| usage("Spark request timeout is too large"))?;
        let route = format!("api/sy.spark/v1/operations/{id}/events");
        let url = self
            .base
            .join(&route)
            .map_err(|error| usage(format!("invalid Spark route: {error}")))?;
        let events = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .header("Last-Event-ID", last_event_id)
            .send()
            .ok()
            .and_then(|response| response.error_for_status().ok())
            .and_then(|response| response.text().ok())
            .map(|text| decode_sse_events(&text))
            .unwrap_or_default();
        if let Some(terminal) = events.iter().rev().find(|event| event.state.is_terminal()) {
            let operation = self.operation(&terminal.operation_id)?;
            return terminal_result(operation);
        }
        loop {
            let operation = self.operation(id)?;
            if operation.state.is_terminal() {
                return terminal_result(operation);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(OPERATION_POLL_INTERVAL));
        }
        Err(unreachable(
            "Spark operation is still running; reconnect with operations --follow",
        ))
    }

    fn mutation_json<B: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        route: &str,
        idempotency_key: &str,
        body: Option<&B>,
    ) -> Result<T, ClientError> {
        if idempotency_key.is_empty() || idempotency_key.len() > 128 {
            return Err(usage("Idempotency-Key must contain 1..128 bytes"));
        }
        let url = self
            .base
            .join(route)
            .map_err(|error| usage(format!("invalid Spark route: {error}")))?;
        let canonical = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| usage("could not encode Spark request"))?;
        for attempt in 0..MAX_SAFE_READ_ATTEMPTS {
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(&self.token)
                .header("Idempotency-Key", idempotency_key);
            if let Some(bytes) = &canonical {
                request = request
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(bytes.clone());
            }
            match request.send() {
                Ok(response) if response.status().is_success() => return decode_response(response),
                Ok(response)
                    if response.status() == StatusCode::UNAUTHORIZED
                        || response.status() == StatusCode::FORBIDDEN =>
                {
                    return Err(unreachable("Spark authentication failed"));
                }
                Ok(response) => return Err(map_problem(response.status(), response.json().ok())),
                Err(_) if attempt + 1 < MAX_SAFE_READ_ATTEMPTS => {
                    thread::sleep(Duration::from_millis(50 * (attempt as u64 + 1)))
                }
                Err(_) => {
                    return Err(unreachable(
                        "Spark mutation acceptance is unreachable; retry with the same idempotency key",
                    ));
                }
            }
        }
        Err(unreachable("Spark agent is unreachable"))
    }
}

fn decode_response<T: serde::de::DeserializeOwned>(response: Response) -> Result<T, ClientError> {
    response
        .json()
        .map_err(|_| internal("Spark returned an incompatible JSON document"))
}

fn decode_sse_events(text: &str) -> Vec<OperationEvent> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect()
}

fn terminal_result(operation: OperationDocument) -> Result<OperationDocument, ClientError> {
    if matches!(
        operation.state,
        super::wire::OperationState::Failed | super::wire::OperationState::Cancelled
    ) {
        let message = operation
            .problem
            .as_ref()
            .map(|problem| problem.detail.clone())
            .unwrap_or_else(|| format!("Spark operation {} did not succeed", operation.id));
        Err(ClientError {
            code: EXIT_OPERATION_FAILED,
            message,
        })
    } else {
        Ok(operation)
    }
}

fn validate_id(id: &str) -> Result<(), ClientError> {
    if id.len() == 26 && id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(usage("Spark resource ID is invalid"))
    }
}

fn valid_model_id(id: &str) -> bool {
    id.len() == 34
        && id.starts_with("m_")
        && id[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_instance_reference(value: &str) -> Result<(), ClientError> {
    if !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        Ok(())
    } else {
        Err(usage("Spark instance reference is invalid"))
    }
}

fn credential_path(config_dir: &Path, value: &str) -> Result<PathBuf, ClientError> {
    let path = Path::new(value);
    if path.is_absolute()
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(usage("Spark credential must be a relative contained path"));
    }
    Ok(config_dir.join("credentials").join(path))
}

fn reject_plaintext_lan(url: &Url) -> Result<(), ClientError> {
    if url.scheme() != "http" {
        return Ok(());
    }
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if loopback {
        Ok(())
    } else {
        Err(unreachable(
            "plaintext Spark HTTP is allowed only on loopback",
        ))
    }
}

#[cfg(unix)]
fn enforce_private_mode(path: &Path) -> Result<(), ClientError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .map_err(|_| unreachable("Spark credential is unavailable"))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(unreachable("Spark credential permissions must be 0600"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_private_mode(_: &Path) -> Result<(), ClientError> {
    Err(usage("Spark credentials require Unix permission semantics"))
}

fn read_text(path: &Path) -> Result<String, ClientError> {
    fs::read_to_string(path).map_err(|_| {
        unreachable(format!(
            "required Spark file {} is unavailable",
            path.display()
        ))
    })
}

fn map_problem(status: StatusCode, problem: Option<ProblemDocument>) -> ClientError {
    let message = problem
        .map(|value| value.detail)
        .unwrap_or_else(|| format!("Spark request failed with HTTP {}", status.as_u16()));
    if status.is_client_error() {
        ClientError {
            code: EXIT_REJECTED,
            message,
        }
    } else {
        internal(message)
    }
}

fn usage(message: impl Into<String>) -> ClientError {
    ClientError {
        code: EXIT_USAGE,
        message: message.into(),
    }
}
fn unreachable(message: impl Into<String>) -> ClientError {
    ClientError {
        code: EXIT_UNREACHABLE,
        message: message.into(),
    }
}
fn internal(message: impl Into<String>) -> ClientError {
    ClientError {
        code: EXIT_INTERNAL,
        message: message.into(),
    }
}

pub fn store_bootstrap(
    config_dir: &Path,
    host: &str,
    url: &str,
    material: &BootstrapMaterial,
) -> Result<(), ClientError> {
    if host.is_empty() || host.contains('/') || host == "." || host == ".." {
        return Err(usage("Spark host profile name is invalid"));
    }
    let ca_dir = config_dir.join("spark");
    let credential_dir = config_dir.join("credentials/spark");
    fs::create_dir_all(&ca_dir)
        .and_then(|()| fs::create_dir_all(&credential_dir))
        .map_err(|_| usage("could not create protected Spark configuration directories"))?;
    write_private_atomic(
        &ca_dir.join(format!("{host}.ca.pem")),
        material.ca_certificate_pem.as_bytes(),
    )?;
    write_private_atomic(&credential_dir.join(host), material.token.as_bytes())?;
    let profile_path = config_dir.join("spark.toml");
    let mut profiles: toml::Value = fs::read_to_string(&profile_path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_else(|| toml::Value::Table(Default::default()));
    let table = profiles
        .as_table_mut()
        .ok_or_else(|| usage("Spark profile root must be a TOML table"))?;
    let hosts = table
        .entry("hosts")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| usage("Spark hosts profile must be a TOML table"))?;
    hosts.insert(
        host.into(),
        toml::Value::try_from(HostProfile {
            url: url.into(),
            ca_cert_sha256: material.ca_certificate_sha256.clone(),
            credential: format!("spark/{host}"),
            request_timeout_seconds: 30,
        })
        .map_err(|_| usage("could not encode Spark host profile"))?,
    );
    write_private_atomic(
        &profile_path,
        toml::to_string_pretty(&profiles)
            .map_err(|_| usage("could not encode Spark profiles"))?
            .as_bytes(),
    )
}

pub fn replace_ca_pin(
    config_dir: &Path,
    host: &str,
    ca_certificate_pem: &str,
    expected_sha256: &str,
) -> Result<(), ClientError> {
    let actual = format!("sha256:{:x}", Sha256::digest(ca_certificate_pem.as_bytes()));
    if actual != expected_sha256 {
        return Err(unreachable(
            "SSH-delivered Spark CA fingerprint differs from its certificate",
        ));
    }
    let profile_path = config_dir.join("spark.toml");
    let mut profiles: toml::Value = toml::from_str(&read_text(&profile_path)?)
        .map_err(|_| usage("invalid Spark host profiles"))?;
    let profile = profiles
        .get_mut("hosts")
        .and_then(toml::Value::as_table_mut)
        .and_then(|hosts| hosts.get_mut(host))
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| usage(format!("Spark host profile {host:?} is not configured")))?;
    profile.insert("ca_cert_sha256".into(), toml::Value::String(actual));
    write_private_atomic(
        &config_dir.join("spark").join(format!("{host}.ca.pem")),
        ca_certificate_pem.as_bytes(),
    )?;
    write_private_atomic(
        &profile_path,
        toml::to_string_pretty(&profiles)
            .map_err(|_| usage("could not encode Spark profiles"))?
            .as_bytes(),
    )
}

#[cfg(unix)]
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), ClientError> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let temporary = path.with_extension(format!("new-{}", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| usage("could not stage protected Spark credential"))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| usage("could not fsync protected Spark credential"))?;
    fs::rename(&temporary, path)
        .map_err(|_| usage("could not atomically install protected Spark credential"))?;
    fs::File::open(path.parent().unwrap_or(Path::new(".")))
        .and_then(|directory| directory.sync_all())
        .map_err(|_| usage("could not fsync protected Spark credential directory"))
}

#[cfg(not(unix))]
fn write_private_atomic(_: &Path, _: &[u8]) -> Result<(), ClientError> {
    Err(usage("Spark credentials require Unix permission semantics"))
}

#[cfg(test)]
mod tests {
    use super::SparkClient;
    use crate::spark::wire::{
        OperationDocument, OperationProgress, OperationState, OPERATION_SCHEMA,
    };
    use sha2::Digest;
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        thread,
        time::Duration,
    };

    #[test]
    fn follow_operation_waits_beyond_three_polls_within_client_timeout() {
        const OPERATION_ID: &str = "01K00000000000000000000000";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for request in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = [0_u8; 2048];
                let _ = stream.read(&mut bytes).unwrap();
                let state = if request == 4 { "succeeded" } else { "running" };
                let body = if request == 0 {
                    String::new()
                } else {
                    serde_json::json!({"schema":"sy.spark.operation/v1","id":OPERATION_ID,"kind":"test","actor_token_id":"admin","target":null,"state":state,"progress":{"stage":"fixture","current":null,"total":null,"unit":null,"message":"waiting"},"created_at":"2026-08-24T00:00:00Z","updated_at":"2026-08-24T00:00:01Z","result":{},"problem":null}).to_string()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let client = SparkClient {
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            base: format!("http://{address}/").parse().unwrap(),
            token: "test".into(),
            request_timeout: Duration::from_secs(2),
        };

        assert_eq!(
            client.follow_operation(OPERATION_ID, 0).unwrap().state,
            OperationState::Succeeded
        );
    }

    #[test]
    fn wrong_pin_plaintext_lan_and_missing_token_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("spark")).unwrap();
        fs::create_dir_all(root.path().join("credentials/spark")).unwrap();
        fs::write(root.path().join("spark/dgx.ca.pem"), b"not a certificate").unwrap();
        fs::write(root.path().join("spark.toml"), "[hosts.dgx]\nurl='https://127.0.0.1:9843'\nca_cert_sha256='sha256:wrong'\ncredential='spark/dgx'\nrequest_timeout_seconds=1\n").unwrap();
        let wrong_pin = SparkClient::load(root.path(), "dgx").unwrap_err();
        assert_eq!(wrong_pin.code, super::EXIT_UNREACHABLE);
        fs::write(root.path().join("spark.toml"), "[hosts.dgx]\nurl='http://10.1.30.143:9843'\nca_cert_sha256='sha256:wrong'\ncredential='spark/dgx'\nrequest_timeout_seconds=1\n").unwrap();
        assert_eq!(
            SparkClient::load(root.path(), "dgx").unwrap_err().code,
            super::EXIT_UNREACHABLE
        );
        let ca = fs::read(root.path().join("spark/dgx.ca.pem")).unwrap();
        let pin = format!("sha256:{:x}", sha2::Sha256::digest(ca));
        fs::write(root.path().join("spark.toml"), format!("[hosts.dgx]\nurl='https://127.0.0.1:9843'\nca_cert_sha256='{pin}'\ncredential='spark/dgx'\nrequest_timeout_seconds=1\n")).unwrap();
        assert_eq!(
            SparkClient::load(root.path(), "dgx").unwrap_err().code,
            super::EXIT_UNREACHABLE
        );
        let leaked = "spark-secret-should-never-leak";
        fs::write(root.path().join("credentials/spark/dgx"), leaked).unwrap();
        fs::set_permissions(
            root.path().join("credentials/spark/dgx"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(!SparkClient::load(root.path(), "dgx")
            .unwrap_err()
            .to_string()
            .contains(leaked));
    }

    #[test]
    fn sse_resume_then_poll_reaches_same_terminal_result() {
        let progress = OperationProgress {
            stage: "complete".into(),
            current: Some(1),
            total: Some(1),
            unit: Some("item".into()),
            message: "done".into(),
        };
        let operation = OperationDocument {
            schema: OPERATION_SCHEMA.into(),
            id: "01K00000000000000000000000".into(),
            kind: "test".into(),
            actor_token_id: "admin".into(),
            target: None,
            state: OperationState::Succeeded,
            progress: progress.clone(),
            created_at: "2026-08-24T00:00:00Z".into(),
            updated_at: "2026-08-24T00:00:01Z".into(),
            result: Some(serde_json::json!({"ok":true})),
            problem: None,
        };
        let event = crate::spark::wire::OperationEvent {
            schema: crate::spark::wire::OPERATION_EVENT_SCHEMA.into(),
            id: 2,
            operation_id: operation.id.clone(),
            state: OperationState::Succeeded,
            progress,
            occurred_at: operation.updated_at.clone(),
        };
        let stream = format!(
            "id: 2\nevent: operation\ndata: {}\n\n",
            serde_json::to_string(&event).unwrap()
        );
        let resumed = super::decode_sse_events(&stream);
        assert_eq!(
            (
                resumed[0].id,
                super::terminal_result(operation.clone()).unwrap()
            ),
            (2, operation)
        );
    }

    #[test]
    fn codex_config_uses_current_provider_keys_without_reading_the_secret() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("spark.toml"), "[hosts.dgx]\nurl='https://10.1.30.143:9843/'\nca_cert_sha256='sha256:x'\ncredential='spark/dgx'\nrequest_timeout_seconds=30\n").unwrap();
        fs::create_dir_all(root.path().join("credentials/spark")).unwrap();
        fs::write(
            root.path().join("credentials/spark/dgx"),
            "never-print-this",
        )
        .unwrap();
        let before = fs::read_dir(root.path()).unwrap().count();
        let config =
            super::codex_client_config(root.path(), "dgx", "ornith", "ornith-1.5:9b").unwrap();
        let rendered = serde_json::to_string(&config).unwrap();
        for key in [
            "base_url",
            "env_key",
            "wire_api",
            "supports_websockets",
            "supports_standalone_web_search",
        ] {
            assert!(rendered.contains(key));
        }
        assert!(!rendered.contains("never-print-this"));
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), before);
        let parsed: toml::Value = toml::from_str(&config.toml).unwrap();
        assert_eq!(parsed["web_search"].as_str(), Some("disabled"));
        assert_eq!(config.base_url, "https://10.1.30.143:9843/openai/ornith/v1");
    }

    #[test]
    fn client_config_places_base_above_v1_and_omits_secret() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("spark.toml"), "[hosts.dgx]\nurl='https://10.1.30.143:9843/'\nca_cert_sha256='sha256:x'\ncredential='spark/dgx'\nrequest_timeout_seconds=30\n").unwrap();
        let config =
            super::claude_code_client_config(root.path(), "dgx", "ornith", "ornith-1.5:9b")
                .unwrap();
        let rendered = serde_json::to_string(&config).unwrap();
        assert_eq!(config.base_url, "https://10.1.30.143:9843/anthropic/ornith");
        assert!(rendered.contains("ANTHROPIC_API_KEY"));
        assert!(rendered.contains("NODE_EXTRA_CA_CERTS"));
        assert!(rendered.contains("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"));
        assert!(!rendered.contains("never-print-this"));
        assert_eq!(config.pinned_version, "2.1.241");
    }
}
