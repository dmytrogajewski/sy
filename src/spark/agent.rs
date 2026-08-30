//! Unprivileged, authenticated HTTPS read plane for the Spark appliance.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use arc_swap::ArcSwap;
use axum::{
    body::{Body, Bytes},
    extract::{
        rejection::BytesRejection, ConnectInfo, DefaultBodyLimit, Extension, Path as AxumPath,
        Query, RawQuery, Request, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{any, delete, get},
    Json, Router,
};
use governor::{clock::DefaultClock, state::keyed::DashMapStateStore, Quota, RateLimiter};
use hmac::{Hmac, Mac};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use utoipa::OpenApi;

use super::wire::{
    AnthropicErrorDetail, AnthropicErrorDocument, AnthropicTokenCountDocument,
    CertificateStatusDocument, DatabaseHealth, DegradedReason, DoctorCheck, DoctorDocument,
    DownloadPlanDocument, DownloadRequest, EngineLogDocument, InstanceDesiredState,
    InstanceDocument, InstanceListDocument, InstanceObservedState, ModelDocument,
    ModelListDocument, OpenAiEmbeddingDocument, OpenAiEmbeddingRequest, OpenAiEmbeddingUsage,
    OpenAiEmbeddingVector, OperationDocument, OperationListDocument, OperationProgress,
    ProblemDocument, RemovalPlanDocument, RemoveModelRequest, ServeAdmissionRequest, ServeRequest,
    StatusDocument, StopRequest, TokenCreateRequest, TokenCreatedDocument, TokenListDocument,
    TokenScope, CERTIFICATE_SCHEMA, DOCTOR_SCHEMA, ENGINE_LOG_SCHEMA, INSTANCE_LIST_SCHEMA,
    INSTANCE_SCHEMA, MODEL_LIST_SCHEMA, OPERATION_LIST_SCHEMA, PROBLEM_SCHEMA, REMOVAL_PLAN_SCHEMA,
    STATUS_SCHEMA, TOKEN_LIST_SCHEMA,
};
use super::{
    executor::{
        CandidateStorage, CandidateStorageInput, ExecutorClient, LogInput, ObservedEngine,
        ReconcileExpectation, StartInstanceInput, StopInstanceInput,
    },
    gateway::{self, PublicAction, RouteLookup, RouteRegistry},
    model::{
        self, Alias, ArtifactSelection, FallbackConfig, HubAcquirer, Repository, Revision,
        TransferFailure,
    },
    reconcile::{decide, DesiredIntent, ReconcileAction, ValidatedObservation},
    resources::{
        evaluate_admission, persistent_set_fits_reboot_envelope, unix_millis, AdmissionRequest,
        CandidateEnvelope, DeclaredEnvelope, ResourcePolicyConfig, TransitionCoordinator,
        TransitionLease, TransitionLeaseError,
    },
    state::{AuthSnapshot, DbActor, QuarantineEvidence, StateError},
    upstream::{GenerationEvent, ObservedRoute, VisionProbe},
    wire::{ExecutorSnapshot, OperationEvent},
};

const API_BASE: &str = "/api/sy.spark/v1";
const ENGINE_READINESS_INTERVAL: Duration = Duration::from_millis(100);
const STOP_COMPLETION_POLL_INTERVAL: Duration = Duration::from_millis(250);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const EVENT_EPOCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
type HmacSha256 = Hmac<Sha256>;
type TokenLimiter = RateLimiter<String, DashMapStateStore<String>, DefaultClock>;
type InferenceSlots = BTreeMap<String, (u32, Arc<tokio::sync::Semaphore>)>;
const DATABASE_QUEUE_CAPACITY: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub schema: String,
    pub listen: SocketAddr,
    pub allowed_client_cidrs: Vec<String>,
    pub plain_http_loopback_only: bool,
    pub executor_socket: PathBuf,
    pub engine_catalog: PathBuf,
    pub model_catalog: PathBuf,
    pub operations: OperationsConfig,
    pub resources: ResourcePolicyConfig,
    pub retention: RetentionConfig,
    pub models: ModelsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationsConfig {
    pub max_parallel_downloads: usize,
    pub max_parallel_starts: usize,
    pub max_parallel_tunes: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    pub operation_days: u32,
    pub database_backups: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    pub cache_root: PathBuf,
    pub endpoint: String,
    pub fallback_executable: PathBuf,
    pub no_progress_seconds: u64,
}

#[derive(Clone)]
pub struct AgentState {
    token: SecretString,
    certificate: CertificateStatusDocument,
    allowed_clients: Vec<Cidr>,
    database: Option<DbActor>,
    auth: Arc<ArcSwap<AuthSnapshot>>,
    limiter: Arc<TokenLimiter>,
    executor: Option<ExecutorClient>,
    engine_catalog: Option<Arc<super::engine::EngineCatalog>>,
    model_catalog: Option<Arc<super::model_catalog::ModelCatalog>>,
    models: Option<Arc<HubAcquirer>>,
    download_slots: Arc<tokio::sync::Semaphore>,
    start_slots: Arc<tokio::sync::Semaphore>,
    inference_slots: Arc<std::sync::Mutex<InferenceSlots>>,
    admission: TransitionCoordinator,
    routes: RouteRegistry,
    #[cfg(test)]
    executor_ready_override: bool,
}

impl AgentState {
    pub fn new(token: impl Into<String>, dns_sans: Vec<String>, ip_sans: Vec<String>) -> Self {
        Self {
            token: SecretString::from(token.into()),
            certificate: CertificateStatusDocument {
                schema: CERTIFICATE_SCHEMA.into(),
                valid: true,
                dns_sans,
                ip_sans,
            },
            allowed_clients: vec![
                "127.0.0.0/8".parse().expect("static loopback CIDR"),
                "::1/128".parse().expect("static loopback CIDR"),
            ],
            database: None,
            auth: Arc::new(ArcSwap::from_pointee(AuthSnapshot::default())),
            limiter: Arc::new(RateLimiter::keyed(Quota::per_second(
                NonZeroU32::new(30).expect("static non-zero quota"),
            ))),
            executor: None,
            engine_catalog: {
                #[cfg(test)]
                {
                    Some(Arc::new(
                        super::engine::EngineCatalog::parse_files([
                            (
                                "llama-cpp.toml",
                                include_str!("../../configs/sy/spark/engines/llama-cpp.toml"),
                            ),
                            (
                                "vllm.toml",
                                include_str!("../../configs/sy/spark/engines/vllm.toml"),
                            ),
                        ])
                        .expect("test engine catalog"),
                    ))
                }
                #[cfg(not(test))]
                {
                    None
                }
            },
            model_catalog: {
                #[cfg(test)]
                {
                    Some(Arc::new(
                        super::model_catalog::ModelCatalog::parse(include_str!(
                            "../../configs/sy/spark/models.toml"
                        ))
                        .expect("test model catalog"),
                    ))
                }
                #[cfg(not(test))]
                {
                    None
                }
            },
            models: None,
            download_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            start_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            inference_slots: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            admission: TransitionCoordinator::new(),
            routes: RouteRegistry::default(),
            #[cfg(test)]
            executor_ready_override: false,
        }
    }

    fn with_engine_catalog(mut self, catalog: super::engine::EngineCatalog) -> Self {
        self.engine_catalog = Some(Arc::new(catalog));
        self
    }

    fn with_model_catalog(mut self, catalog: super::model_catalog::ModelCatalog) -> Self {
        self.model_catalog = Some(Arc::new(catalog));
        self
    }

    fn inference_slot(&self, id: &str, maximum: u32) -> Arc<tokio::sync::Semaphore> {
        let mut slots = self
            .inference_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = slots.entry(id.into()).or_insert_with(|| {
            (
                maximum,
                Arc::new(tokio::sync::Semaphore::new(maximum as usize)),
            )
        });
        if entry.0 != maximum {
            *entry = (
                maximum,
                Arc::new(tokio::sync::Semaphore::new(maximum as usize)),
            );
        }
        Arc::clone(&entry.1)
    }

    fn store_auth(&self, snapshot: AuthSnapshot) {
        self.inference_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|id, _| id == "bootstrap-admin" || snapshot.tokens.contains_key(id));
        self.auth.store(Arc::new(snapshot));
    }

    fn with_allowed_clients(mut self, allowed: Vec<Cidr>) -> Self {
        self.allowed_clients = allowed;
        self
    }

    async fn with_database(mut self, database: DbActor) -> Result<Self, StateError> {
        self.store_auth(database.auth_snapshot().await?);
        self.database = Some(database);
        Ok(self)
    }

    fn with_executor(mut self, executor: ExecutorClient) -> Self {
        self.executor = Some(executor);
        self
    }

    fn with_start_slots(mut self, max_parallel: usize) -> Self {
        self.start_slots = Arc::new(tokio::sync::Semaphore::new(max_parallel));
        self
    }

    fn with_models(mut self, models: HubAcquirer, max_parallel: usize) -> Self {
        self.models = Some(Arc::new(models));
        self.download_slots = Arc::new(tokio::sync::Semaphore::new(max_parallel));
        self
    }

    #[cfg(test)]
    fn with_ready_executor_for_test(mut self) -> Self {
        self.executor_ready_override = true;
        self
    }
}

#[derive(Clone)]
struct Cidr {
    network: std::net::IpAddr,
    prefix: u8,
}

impl std::str::FromStr for Cidr {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = value
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("CIDR requires a prefix"))?;
        let network: std::net::IpAddr = address.parse()?;
        let prefix: u8 = prefix.parse()?;
        anyhow::ensure!(
            prefix <= if network.is_ipv4() { 32 } else { 128 },
            "CIDR prefix is out of range"
        );
        Ok(Self { network, prefix })
    }
}

impl Cidr {
    fn contains(&self, address: std::net::IpAddr) -> bool {
        match (self.network, address) {
            (std::net::IpAddr::V4(network), std::net::IpAddr::V4(address)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                u32::from(network) & mask == u32::from(address) & mask
            }
            (std::net::IpAddr::V6(network), std::net::IpAddr::V6(address)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                u128::from(network) & mask == u128::from(address) & mask
            }
            _ => false,
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    paths(
        status,
        doctor,
        certificate_status,
        metrics,
        list_operations,
        get_operation,
        operation_events,
        cancel_operation,
        create_token,
        list_tokens,
        revoke_token,
        list_models,
        get_model,
        download_model,
        remove_model,
        admission,
        serve_instance,
        list_instances,
        stop_instance,
        instance_logs,
        gateway_models,
        gateway_completions,
        gateway_chat_completions,
        gateway_responses,
        gateway_embeddings,
        gateway_anthropic_messages,
        gateway_anthropic_count_tokens
    ),
    components(schemas(
        StatusDocument,
        DoctorDocument,
        CertificateStatusDocument,
        ProblemDocument,
        OperationDocument,
        OperationListDocument,
        OperationEvent,
        TokenCreateRequest,
        TokenCreatedDocument,
        TokenListDocument,
        DownloadRequest,
        DownloadPlanDocument,
        ModelDocument,
        ModelListDocument,
        RemoveModelRequest,
        RemovalPlanDocument,
        ServeAdmissionRequest,
        ServeRequest,
        StopRequest,
        InstanceDocument,
        InstanceListDocument,
        EngineLogDocument,
        AnthropicTokenCountDocument,
        AnthropicErrorDocument,
        AnthropicErrorDetail,
        OpenAiEmbeddingRequest,
        OpenAiEmbeddingDocument,
        OpenAiEmbeddingVector,
        OpenAiEmbeddingUsage,
        crate::spark::resources::AdmissionReport
    ))
)]
struct ApiDoc;

pub fn router(state: AgentState) -> Router {
    let authenticated = Router::new()
        .route(&format!("{API_BASE}/status"), get(status))
        .route(&format!("{API_BASE}/doctor"), get(doctor))
        .route(&format!("{API_BASE}/metrics"), get(metrics))
        .route(
            &format!("{API_BASE}/certificates/status"),
            get(certificate_status),
        )
        .route(&format!("{API_BASE}/operations"), get(list_operations))
        .route(&format!("{API_BASE}/models"), get(list_models))
        .route(
            &format!("{API_BASE}/downloads"),
            axum::routing::post(download_model),
        )
        .route(
            &format!("{API_BASE}/admission"),
            axum::routing::post(admission),
        )
        .route(
            &format!("{API_BASE}/instances"),
            get(list_instances).post(serve_instance),
        )
        .route(
            &format!("{API_BASE}/instances/{{id}}"),
            delete(stop_instance),
        )
        .route(
            &format!("{API_BASE}/instances/{{id}}/logs"),
            get(instance_logs),
        )
        .route(
            &format!("{API_BASE}/models/{{id}}"),
            get(get_model).delete(remove_model),
        )
        .route(
            &format!("{API_BASE}/operations/{{id}}"),
            get(get_operation).delete(cancel_operation),
        )
        .route(
            &format!("{API_BASE}/operations/{{id}}/events"),
            get(operation_events),
        )
        .route(
            &format!("{API_BASE}/tokens"),
            get(list_tokens).post(create_token),
        )
        .route(&format!("{API_BASE}/tokens/{{id}}"), delete(revoke_token))
        .route("/openai/{instance}/v1/models", get(gateway_models))
        .route(
            "/openai/{instance}/v1/completions",
            axum::routing::post(gateway_completions),
        )
        .route(
            "/openai/{instance}/v1/chat/completions",
            axum::routing::post(gateway_chat_completions),
        )
        .route(
            "/openai/{instance}/v1/responses",
            axum::routing::post(gateway_responses),
        )
        .route(
            "/openai/{instance}/v1/embeddings",
            axum::routing::post(gateway_embeddings),
        )
        .route("/openai/{instance}/v1/{*path}", any(openai_not_found))
        .route(
            "/anthropic/{instance}/v1/messages",
            axum::routing::post(gateway_anthropic_messages),
        )
        .route(
            "/anthropic/{instance}/v1/messages/count_tokens",
            axum::routing::post(gateway_anthropic_count_tokens),
        )
        .route("/anthropic/{instance}/{*path}", any(anthropic_not_found))
        .layer(DefaultBodyLimit::max(gateway::MAX_COMPLETION_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    authenticated.fallback(not_found).with_state(state)
}

async fn openai_not_found() -> Response {
    openai_error(
        StatusCode::NOT_FOUND,
        gateway::OpenAiError {
            code: "route_not_found",
            message: "OpenAI route is not implemented",
        },
    )
}

async fn anthropic_not_found() -> Response {
    anthropic_error(
        StatusCode::NOT_FOUND,
        gateway::AnthropicError {
            error_type: "not_found_error",
            message: "Anthropic route is not implemented",
        },
    )
}

fn anthropic_error(status: StatusCode, error: gateway::AnthropicError) -> Response {
    (
        status,
        Json(AnthropicErrorDocument {
            kind: "error".into(),
            error: AnthropicErrorDetail {
                kind: error.error_type.into(),
                message: error.message.into(),
            },
        }),
    )
        .into_response()
}

fn valid_anthropic_query(query: Option<String>) -> bool {
    query
        .as_deref()
        .is_none_or(|query| query.is_empty() || query == "beta=true")
}

fn valid_anthropic_headers(headers: &HeaderMap) -> bool {
    const BETAS: &[&str] = &[
        "claude-code-20250219",
        "interleaved-thinking-2025-05-14",
        "thinking-token-count-2026-05-13",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
        "mid-conversation-system-2026-04-07",
        "effort-2025-11-24",
        "afk-mode-2026-01-31",
    ];
    valid_inference_headers(headers)
        && headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok())
            == Some("2023-06-01")
        && headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| value.split(',').all(|beta| BETAS.contains(&beta.trim())))
}

#[utoipa::path(post, path = "/anthropic/{instance}/v1/messages", params(("instance" = String, Path)), responses((status = 200)))]
async fn gateway_anthropic_messages(
    State(state): State<AgentState>,
    AxumPath(instance): AxumPath<String>,
    RawQuery(query): RawQuery,
    Extension(auth): Extension<AuthenticatedToken>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if !valid_anthropic_query(query) || !valid_anthropic_headers(&headers) {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            gateway::AnthropicError {
                error_type: "invalid_request_error",
                message: "request headers, query, or beta capability are invalid",
            },
        );
    }
    let body = match body {
        Ok(body) => body,
        Err(_) => {
            return anthropic_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                gateway::AnthropicError {
                    error_type: "invalid_request_error",
                    message: "request body is too large",
                },
            );
        }
    };
    let route = match state.routes.lookup(&instance) {
        RouteLookup::Healthy(route) => route,
        RouteLookup::Warming => return anthropic_warming(),
        RouteLookup::Missing => return anthropic_not_found().await,
    };
    if !route.profile.allows(PublicAction::Responses) {
        return anthropic_not_found().await;
    }
    let request = match gateway::rewrite_anthropic_request_with_profile(
        &body,
        &route.served_model,
        &route.profile,
    ) {
        Ok(request) => request,
        Err(error) => return anthropic_error(StatusCode::BAD_REQUEST, error),
    };
    let token_permit = match auth.acquire_inference().await {
        Ok(permit) => permit,
        Err(()) => return anthropic_upstream_unavailable(),
    };
    let permit = match route.acquire().await {
        Ok(permit) => permit,
        Err(_) => return anthropic_upstream_unavailable(),
    };
    let upstream = match route
        .upstream
        .chat_stream_with_idle_timeout(&request.body, route.profile.stream_idle_timeout())
        .await
    {
        Ok(upstream) => upstream,
        Err(_) => return anthropic_upstream_unavailable(),
    };
    let encoder = if request.omit_reasoning {
        gateway::AnthropicEncoder::with_omitted_reasoning(route.public_model.clone())
    } else {
        gateway::AnthropicEncoder::new(route.public_model.clone())
    };
    if request.stream {
        anthropic_sse(upstream, encoder, permit, token_permit)
    } else {
        anthropic_json(upstream, encoder, permit, token_permit).await
    }
}

#[utoipa::path(post, path = "/anthropic/{instance}/v1/messages/count_tokens", params(("instance" = String, Path)), responses((status = 200)))]
async fn gateway_anthropic_count_tokens(
    State(state): State<AgentState>,
    AxumPath(instance): AxumPath<String>,
    RawQuery(query): RawQuery,
    Extension(auth): Extension<AuthenticatedToken>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if !valid_anthropic_query(query) || !valid_anthropic_headers(&headers) {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            gateway::AnthropicError {
                error_type: "invalid_request_error",
                message: "request headers, query, or beta capability are invalid",
            },
        );
    }
    let body = match body {
        Ok(body) => body,
        Err(_) => {
            return anthropic_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                gateway::AnthropicError {
                    error_type: "invalid_request_error",
                    message: "request body is too large",
                },
            );
        }
    };
    let route = match state.routes.lookup(&instance) {
        RouteLookup::Healthy(route) => route,
        RouteLookup::Warming => return anthropic_warming(),
        RouteLookup::Missing => return anthropic_not_found().await,
    };
    if !route.profile.allows(PublicAction::Responses) {
        return anthropic_not_found().await;
    }
    let native_body =
        match gateway::rewrite_anthropic_native_count_request(&body, &route.served_model) {
            Ok(body) => body,
            Err(error) => return anthropic_error(StatusCode::BAD_REQUEST, error),
        };
    let tokenizer_body = match gateway::rewrite_anthropic_count_request(&body, &route.served_model)
    {
        Ok(body) => body,
        Err(error) => return anthropic_error(StatusCode::BAD_REQUEST, error),
    };
    let _token_permit = match auth.acquire_inference().await {
        Ok(permit) => permit,
        Err(()) => return anthropic_upstream_unavailable(),
    };
    let _permit = match route.acquire().await {
        Ok(permit) => permit,
        Err(_) => return anthropic_upstream_unavailable(),
    };
    let (request, upstream_body) =
        match route
            .upstream
            .request("POST", "/v1/messages/count_tokens", native_body.len())
        {
            Ok(request) => (request, native_body),
            Err(_) => match route
                .upstream
                .request("POST", "/tokenize", tokenizer_body.len())
            {
                Ok(request) => (request, tokenizer_body),
                Err(_) => return anthropic_upstream_unavailable(),
            },
        };
    match route.upstream.send(&request, &upstream_body).await {
        Ok(response) if (200..300).contains(&response.status) => {
            gateway::rewrite_anthropic_count_response(&response.bytes)
                .map(|input_tokens| AnthropicTokenCountDocument { input_tokens })
                .map(Json)
                .map(IntoResponse::into_response)
                .unwrap_or_else(|error| anthropic_error(StatusCode::BAD_GATEWAY, error))
        }
        _ => anthropic_upstream_unavailable(),
    }
}

async fn anthropic_json(
    mut upstream: super::upstream::CompletionStream,
    mut encoder: gateway::AnthropicEncoder,
    _permit: tokio::sync::OwnedSemaphorePermit,
    _token_permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    while let Some(event) = upstream.next().await {
        match event {
            Ok(GenerationEvent::Done) => {
                return match encoder.accept(GenerationEvent::Done) {
                    Ok(()) => Json(encoder.final_document()).into_response(),
                    Err(error) => anthropic_error(StatusCode::BAD_GATEWAY, error),
                };
            }
            Ok(event) => {
                if encoder.accept(event).is_err() {
                    return anthropic_upstream_unavailable();
                }
            }
            Err(_) => return anthropic_upstream_unavailable(),
        }
    }
    anthropic_upstream_unavailable()
}

fn anthropic_sse(
    upstream: super::upstream::CompletionStream,
    encoder: gateway::AnthropicEncoder,
    permit: tokio::sync::OwnedSemaphorePermit,
    token_permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let stream = futures_util::stream::unfold(
        (upstream, encoder, permit, token_permit, false),
        |(mut upstream, mut encoder, permit, token_permit, mut terminal)| async move {
            loop {
                if let Some(event) = encoder.pop() {
                    return Some((
                        Ok::<_, std::convert::Infallible>(
                            Event::default()
                                .event(event.name)
                                .data(event.data.to_string()),
                        ),
                        (upstream, encoder, permit, token_permit, terminal),
                    ));
                }
                if terminal {
                    return None;
                }
                match upstream.next().await {
                    Some(Ok(event)) => {
                        terminal = matches!(event, GenerationEvent::Done);
                        if encoder.accept(event).is_err() {
                            encoder.fail();
                            terminal = true;
                        }
                    }
                    _ => {
                        encoder.fail();
                        terminal = true;
                    }
                }
            }
        },
    );
    Sse::new(stream).into_response()
}

fn anthropic_warming() -> Response {
    let mut response = anthropic_error(
        StatusCode::SERVICE_UNAVAILABLE,
        gateway::AnthropicError {
            error_type: "overloaded_error",
            message: "instance is warming or recovering",
        },
    );
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_static(gateway::RETRY_AFTER_SECONDS),
    );
    response
}

fn anthropic_upstream_unavailable() -> Response {
    anthropic_error(
        StatusCode::SERVICE_UNAVAILABLE,
        gateway::AnthropicError {
            error_type: "api_error",
            message: "healthy generation became unavailable",
        },
    )
}

#[utoipa::path(get, path = "/openai/{instance}/v1/models", params(("instance" = String, Path)), responses((status = 200)))]
async fn gateway_models(
    State(state): State<AgentState>,
    AxumPath(instance): AxumPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    if gateway::public_action("GET", "models") != Some(PublicAction::Models) {
        return not_found().await;
    }
    match state.routes.lookup(&instance) {
        RouteLookup::Healthy(route) if route.profile.allows(PublicAction::Models) => {
            Json(gateway::models_document(&route)).into_response()
        }
        RouteLookup::Healthy(_) => not_found().await,
        RouteLookup::Warming => gateway_warming(),
        RouteLookup::Missing => not_found().await,
    }
}

#[utoipa::path(post, path = "/openai/{instance}/v1/completions", params(("instance" = String, Path)), responses((status = 200)))]
async fn gateway_completions(
    State(state): State<AgentState>,
    AxumPath(instance): AxumPath<String>,
    RawQuery(query): RawQuery,
    Extension(auth): Extension<AuthenticatedToken>,
    body: Bytes,
) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    if gateway::public_action("POST", "completions") != Some(PublicAction::Completions) {
        return not_found().await;
    }
    let route = match state.routes.lookup(&instance) {
        RouteLookup::Healthy(route) => route,
        RouteLookup::Warming => return gateway_warming(),
        RouteLookup::Missing => return not_found().await,
    };
    if !route.profile.allows(PublicAction::Completions) {
        return not_found().await;
    }
    let (upstream_body, streaming) =
        match gateway::rewrite_completion_request(&body, &route.served_model) {
            Ok(value) => value,
            Err(()) => {
                return problem(
                    StatusCode::BAD_REQUEST,
                    "spark.inference.invalid-request",
                    "completion request is invalid or too large",
                );
            }
        };
    let token_permit = match auth.acquire_inference().await {
        Ok(permit) => permit,
        Err(()) => return gateway_upstream_unavailable(),
    };
    if streaming {
        let upstream = match route.upstream.completion_stream(&upstream_body).await {
            Ok(stream) => stream,
            Err(_) => return gateway_upstream_unavailable(),
        };
        let public_model = route.public_model.clone();
        let events = futures_util::stream::unfold(
            (upstream, token_permit),
            move |(mut upstream, permit)| {
                let public_model = public_model.clone();
                async move {
                    let event = upstream.next().await?;
                    let data = match event {
                        Ok(GenerationEvent::TextDelta { text }) => serde_json::json!({
                            "object": "text_completion",
                            "model": public_model,
                            "choices": [{"index": 0, "text": text, "finish_reason": null}]
                        }),
                        Ok(GenerationEvent::ReasoningDelta { text }) => serde_json::json!({
                            "object": "text_completion.reasoning",
                            "model": public_model,
                            "reasoning_content": text
                        }),
                        Ok(GenerationEvent::Finished { finish_reason }) => serde_json::json!({
                            "object": "text_completion",
                            "model": public_model,
                            "choices": [{"index": 0, "text": "", "finish_reason": finish_reason}]
                        }),
                        Ok(GenerationEvent::Usage {
                            prompt_tokens,
                            completion_tokens,
                        }) => serde_json::json!({
                            "object": "text_completion.usage",
                            "model": public_model,
                            "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens}
                        }),
                        Ok(GenerationEvent::ToolCallDelta { .. }) => serde_json::json!({
                            "error": {"type": "unsupported_output", "message": "tool calls require chat or responses"}
                        }),
                        Ok(GenerationEvent::Done) => serde_json::Value::String("[DONE]".into()),
                        Err(_) => serde_json::json!({
                            "error": {"type": "spark_upstream_error", "message": "upstream stream ended"}
                        }),
                    };
                    let data = data
                        .as_str()
                        .map_or_else(|| data.to_string(), str::to_owned);
                    Some((
                        Ok::<Event, std::convert::Infallible>(Event::default().data(data)),
                        (upstream, permit),
                    ))
                }
            },
        );
        return Sse::new(events).into_response();
    }
    let request = match route
        .upstream
        .request("POST", "/v1/completions", upstream_body.len())
    {
        Ok(request) => request,
        Err(_) => return gateway_upstream_unavailable(),
    };
    match route.upstream.send(&request, &upstream_body).await {
        Ok(response) if (200..300).contains(&response.status) => {
            match gateway::rewrite_completion_response(&response.bytes, &route.public_model) {
                Ok(document) => Json(document).into_response(),
                Err(()) => gateway_upstream_unavailable(),
            }
        }
        _ => gateway_upstream_unavailable(),
    }
}

fn openai_error(status: StatusCode, error: gateway::OpenAiError) -> Response {
    let error_type = if error.code == "server_error" {
        "server_error"
    } else {
        "invalid_request_error"
    };
    let body = serde_json::json!({"error": {"message": error.message,
        "type": error_type, "param": null, "code": error.code}});
    (status, Json(body)).into_response()
}

fn valid_inference_headers(headers: &HeaderMap) -> bool {
    headers.len() <= 64
        && headers
            .iter()
            .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
            .sum::<usize>()
            <= 16 * 1024
        && !headers.keys().any(|name| {
            matches!(
                name.as_str(),
                "forwarded"
                    | "x-forwarded-for"
                    | "x-forwarded-host"
                    | "x-forwarded-proto"
                    | "x-real-ip"
            )
        })
}

#[utoipa::path(post, path = "/openai/{instance}/v1/responses", params(("instance" = String, Path)), responses((status = 200)))]
async fn gateway_responses(
    State(state): State<AgentState>,
    AxumPath(instance): AxumPath<String>,
    RawQuery(query): RawQuery,
    Extension(auth): Extension<AuthenticatedToken>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if reject_query(query).is_some()
        || !valid_inference_headers(&headers)
        || gateway::public_action("POST", "responses") != Some(PublicAction::Responses)
    {
        return openai_error(
            StatusCode::BAD_REQUEST,
            gateway::OpenAiError {
                code: "invalid_request_error",
                message: "request headers, query, or route are invalid",
            },
        );
    }
    let route = match state.routes.lookup(&instance) {
        RouteLookup::Healthy(route) => route,
        RouteLookup::Warming => return gateway_warming(),
        RouteLookup::Missing => return not_found().await,
    };
    if !route.profile.allows(PublicAction::Responses) {
        return openai_not_found().await;
    }
    if route.profile.native_responses {
        let request = match gateway::rewrite_native_responses_request_with_profile(
            &body,
            &route.public_model,
            &route.profile,
        ) {
            Ok(request) => request,
            Err(error) => return openai_error(StatusCode::BAD_REQUEST, error),
        };
        let token_permit = match auth.acquire_inference().await {
            Ok(permit) => permit,
            Err(()) => return gateway_upstream_unavailable(),
        };
        return gateway_native_responses(route, request, token_permit).await;
    }
    let request = match gateway::rewrite_responses_request_with_profile(
        &body,
        &route.served_model,
        &route.profile,
    ) {
        Ok(request) => request,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, error),
    };
    let token_permit = match auth.acquire_inference().await {
        Ok(permit) => permit,
        Err(()) => return gateway_upstream_unavailable(),
    };
    gateway_responses_stream(route, request, token_permit).await
}

async fn gateway_native_responses(
    route: Arc<gateway::HealthyRoute>,
    request: gateway::NativeResponsesRequest,
    token_permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let permit = match route.acquire().await {
        Ok(permit) => permit,
        Err(error) => return openai_error(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    if request.stream {
        let upstream = match route
            .upstream
            .raw_stream("/v1/responses", &request.body)
            .await
        {
            Ok(upstream) => upstream,
            Err(_) => return gateway_upstream_unavailable(),
        };
        let stream = futures_util::stream::unfold(
            (upstream, permit, token_permit),
            |(mut upstream, permit, token_permit)| async move {
                let chunk = upstream.next().await?.ok()?;
                Some((
                    Ok::<_, std::convert::Infallible>(chunk),
                    (upstream, permit, token_permit),
                ))
            },
        );
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| gateway_upstream_unavailable());
    }
    let upstream_request = match route
        .upstream
        .request("POST", "/v1/responses", request.body.len())
    {
        Ok(request) => request,
        Err(_) => return gateway_upstream_unavailable(),
    };
    let response = route
        .upstream
        .send_with_timeout(
            &upstream_request,
            &request.body,
            Duration::from_secs(route.profile.native_response_timeout_seconds),
        )
        .await;
    drop(permit);
    drop(token_permit);
    match response {
        Ok(response) => Response::builder()
            .status(
                StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(response.bytes))
            .unwrap_or_else(|_| gateway_upstream_unavailable()),
        Err(_) => gateway_upstream_unavailable(),
    }
}

#[utoipa::path(post, path = "/openai/{instance}/v1/embeddings", params(("instance" = String, Path)), request_body = OpenAiEmbeddingRequest, responses((status = 200, body = OpenAiEmbeddingDocument)))]
async fn gateway_embeddings(
    State(state): State<AgentState>,
    AxumPath(instance): AxumPath<String>,
    RawQuery(query): RawQuery,
    Extension(auth): Extension<AuthenticatedToken>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if reject_query(query).is_some()
        || !valid_inference_headers(&headers)
        || gateway::public_action("POST", "embeddings") != Some(PublicAction::Embeddings)
    {
        return openai_error(
            StatusCode::BAD_REQUEST,
            gateway::OpenAiError {
                code: "invalid_request_error",
                message: "request headers, query, or route are invalid",
            },
        );
    }
    let route = match state.routes.lookup(&instance) {
        RouteLookup::Healthy(route) => route,
        RouteLookup::Warming => return gateway_warming(),
        RouteLookup::Missing => return openai_not_found().await,
    };
    if !route.profile.allows(PublicAction::Embeddings) {
        return openai_not_found().await;
    }
    let rewritten =
        match gateway::rewrite_embeddings_request(&body, &route.served_model, &route.profile) {
            Ok(request) => request,
            Err(error) => return openai_error(StatusCode::BAD_REQUEST, error),
        };
    let _token_permit = match auth.acquire_inference().await {
        Ok(permit) => permit,
        Err(()) => return gateway_upstream_unavailable(),
    };
    let _permit = match route.acquire().await {
        Ok(permit) => permit,
        Err(error) => return openai_error(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let request = match route
        .upstream
        .request("POST", "/v1/embeddings", rewritten.body.len())
    {
        Ok(request) => request,
        Err(_) => return gateway_upstream_unavailable(),
    };
    match route.upstream.send(&request, &rewritten.body).await {
        Ok(response) if (200..300).contains(&response.status) => {
            gateway::rewrite_embeddings_response(
                &response.bytes,
                &route.public_model,
                &route.served_model,
                &route.profile,
                rewritten.input_count,
            )
            .map(Json)
            .map(IntoResponse::into_response)
            .unwrap_or_else(|error| openai_error(StatusCode::BAD_GATEWAY, error))
        }
        _ => gateway_upstream_unavailable(),
    }
}

#[utoipa::path(post, path = "/openai/{instance}/v1/chat/completions", params(("instance" = String, Path)), responses((status = 200)))]
async fn gateway_chat_completions(
    State(state): State<AgentState>,
    AxumPath(instance): AxumPath<String>,
    RawQuery(query): RawQuery,
    Extension(auth): Extension<AuthenticatedToken>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if reject_query(query).is_some()
        || !valid_inference_headers(&headers)
        || gateway::public_action("POST", "chat/completions") != Some(PublicAction::Chat)
    {
        return openai_error(
            StatusCode::BAD_REQUEST,
            gateway::OpenAiError {
                code: "invalid_request_error",
                message: "request headers or query are invalid",
            },
        );
    }
    let route = match state.routes.lookup(&instance) {
        RouteLookup::Healthy(route) => route,
        RouteLookup::Warming => return gateway_warming(),
        RouteLookup::Missing => return not_found().await,
    };
    if !route.profile.allows(PublicAction::Chat) {
        return openai_not_found().await;
    }
    let request = match gateway::rewrite_chat_request_with_profile(
        &body,
        &route.served_model,
        &route.profile,
    ) {
        Ok(request) => request,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, error),
    };
    let token_permit = match auth.acquire_inference().await {
        Ok(permit) => permit,
        Err(()) => return gateway_upstream_unavailable(),
    };
    gateway_chat(route, request, token_permit).await
}

async fn gateway_chat(
    route: Arc<gateway::HealthyRoute>,
    request: gateway::GenerationRequest,
    token_permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let permit = match route.acquire().await {
        Ok(permit) => permit,
        Err(error) => return openai_error(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    if request.stream {
        let upstream = match route
            .upstream
            .chat_stream_with_idle_timeout(&request.body, route.profile.stream_idle_timeout())
            .await
        {
            Ok(upstream) => upstream,
            Err(_) => return gateway_upstream_unavailable(),
        };
        return chat_sse(upstream, route.public_model.clone(), permit, token_permit);
    }
    let upstream_request =
        match route
            .upstream
            .request("POST", "/v1/chat/completions", request.body.len())
        {
            Ok(request) => request,
            Err(_) => return gateway_upstream_unavailable(),
        };
    let response = route.upstream.send(&upstream_request, &request.body).await;
    drop(permit);
    match response {
        Ok(response) if (200..300).contains(&response.status) => {
            gateway::rewrite_chat_response(&response.bytes, &route.public_model)
                .map(Json)
                .map(IntoResponse::into_response)
                .unwrap_or_else(|error| openai_error(StatusCode::BAD_GATEWAY, error))
        }
        _ => gateway_upstream_unavailable(),
    }
}

fn chat_sse(
    upstream: super::upstream::CompletionStream,
    model: String,
    permit: tokio::sync::OwnedSemaphorePermit,
    token_permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let id = format!(
        "chatcmpl_{}",
        ulid::Ulid::new().to_string().to_ascii_lowercase()
    );
    let stream = futures_util::stream::unfold(
        (upstream, model, id, permit, token_permit),
        |(mut upstream, model, id, permit, token_permit)| async move {
            let event = upstream.next().await?;
            let data = match event {
                Ok(GenerationEvent::Done) => "[DONE]".into(),
                Ok(event) => gateway::chat_stream_document(event, &id, &model)?.to_string(),
                Err(_) => serde_json::json!({"error":{"type":"server_error","message":"upstream stream failed"}}).to_string(),
            };
            Some((
                Ok::<_, std::convert::Infallible>(Event::default().data(data)),
                (upstream, model, id, permit, token_permit),
            ))
        },
    );
    Sse::new(stream).into_response()
}

async fn gateway_responses_stream(
    route: Arc<gateway::HealthyRoute>,
    request: gateway::GenerationRequest,
    token_permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let permit = match route.acquire().await {
        Ok(permit) => permit,
        Err(error) => return openai_error(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let upstream = match route
        .upstream
        .chat_stream_with_idle_timeout(&request.body, route.profile.stream_idle_timeout())
        .await
    {
        Ok(upstream) => upstream,
        Err(_) => return gateway_upstream_unavailable(),
    };
    let encoder = gateway::ResponsesEncoder::new(route.public_model.clone(), request.custom_tools);
    if request.stream {
        responses_sse(upstream, encoder, permit, token_permit)
    } else {
        responses_json(upstream, encoder, permit, token_permit).await
    }
}

async fn responses_json(
    mut upstream: super::upstream::CompletionStream,
    mut encoder: gateway::ResponsesEncoder,
    _permit: tokio::sync::OwnedSemaphorePermit,
    _token_permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let mut done = false;
    while let Some(event) = upstream.next().await {
        match event {
            Ok(GenerationEvent::Done) => {
                let _ = encoder.accept(GenerationEvent::Done);
                done = true;
                break;
            }
            Ok(event) => {
                if encoder.accept(event).is_err() {
                    return gateway_upstream_unavailable();
                }
            }
            _ => return gateway_upstream_unavailable(),
        }
    }
    if done {
        Json(encoder.final_document()).into_response()
    } else {
        gateway_upstream_unavailable()
    }
}

fn responses_sse(
    upstream: super::upstream::CompletionStream,
    encoder: gateway::ResponsesEncoder,
    permit: tokio::sync::OwnedSemaphorePermit,
    token_permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let stream = futures_util::stream::unfold(
        (upstream, encoder, permit, token_permit, false),
        |(mut upstream, mut encoder, permit, token_permit, mut terminal)| async move {
            loop {
                if let Some(event) = encoder.pop() {
                    let frame = Event::default()
                        .event(event.name)
                        .data(event.data.to_string());
                    return Some((
                        Ok::<_, std::convert::Infallible>(frame),
                        (upstream, encoder, permit, token_permit, terminal),
                    ));
                }
                if terminal {
                    return None;
                }
                match upstream.next().await {
                    Some(Ok(event)) => {
                        terminal = matches!(event, GenerationEvent::Done);
                        let _ = encoder.accept(event);
                    }
                    _ => {
                        encoder.fail();
                        terminal = true;
                    }
                }
            }
        },
    );
    Sse::new(stream).into_response()
}

fn gateway_warming() -> Response {
    let mut response = openai_error(
        StatusCode::SERVICE_UNAVAILABLE,
        gateway::OpenAiError {
            code: "model_not_ready",
            message: "instance is warming or recovering and has not passed semantic readiness",
        },
    );
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_static(gateway::RETRY_AFTER_SECONDS),
    );
    response
}

fn gateway_upstream_unavailable() -> Response {
    openai_error(
        StatusCode::SERVICE_UNAVAILABLE,
        gateway::OpenAiError {
            code: "server_error",
            message: "healthy generation became unavailable",
        },
    )
}
#[utoipa::path(post, path = "/api/sy.spark/v1/admission", request_body = ServeAdmissionRequest, responses((status = 200, body = crate::spark::resources::AdmissionReport)))]
async fn admission(
    State(state): State<AgentState>,
    Json(body): Json<ServeAdmissionRequest>,
) -> Response {
    if let Err(response) = require_executor_for_mutation(&state).await {
        return response;
    }
    if !body.dry_run {
        return problem(
            StatusCode::BAD_REQUEST,
            "spark.request.dry-run-required",
            "resource admission preview requires dry_run=true",
        );
    }
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    let model = match database.model(&body.model).await {
        Ok(model) => model,
        Err(error) => return state_problem(error),
    };
    let lease_id = format!("admission:{}", model.id);
    let _lease = match state.admission.try_acquire(lease_id) {
        Ok(lease) => lease,
        Err(TransitionLeaseError::Busy { .. }) => {
            return problem(
                StatusCode::CONFLICT,
                "spark.memory.transition-busy",
                "another high-memory transition owns the admission lease",
            );
        }
        Err(TransitionLeaseError::Unavailable) => {
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "spark.memory.admission-unavailable",
                "the admission coordinator is unavailable",
            );
        }
    };
    let Some(executor) = &state.executor else {
        return executor_unavailable();
    };
    let Some(catalog) = &state.engine_catalog else {
        return executor_unavailable();
    };
    let Some(artifacts) = model.artifacts.as_ref() else {
        return problem(
            StatusCode::CONFLICT,
            "spark.engine.artifacts-required",
            "model has no verified artifact traits for engine selection",
        );
    };
    let policy = match catalog.select(artifacts) {
        Ok(policy) => policy,
        Err(error) => return problem(StatusCode::CONFLICT, "spark.engine.unsupported", &error),
    };
    let artifact_fingerprint = match super::engine::artifact_fingerprint(artifacts) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            return problem(
                StatusCode::CONFLICT,
                "spark.engine.artifacts-invalid",
                &error,
            )
        }
    };
    let snapshot = match executor.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(_) => return executor_unavailable(),
    };
    let resources = match policy.resources_for(None, artifacts) {
        Ok(resources) => resources,
        Err(error) => return problem(StatusCode::CONFLICT, "spark.engine.profile-invalid", &error),
    };
    let startup_peak = resources.startup_peak_bytes;
    let desired: Vec<DeclaredEnvelope> = match database.desired_resource_envelopes().await {
        Ok(envelopes) => envelopes,
        Err(error) => return state_problem(error),
    };
    let Some(candidate_name) = normalize_instance_name(
        body.name
            .as_deref()
            .or_else(|| model.aliases.first().map(String::as_str))
            .unwrap_or(&model.id),
    ) else {
        return problem(
            StatusCode::BAD_REQUEST,
            "spark.instance.invalid-name",
            "instance name is invalid",
        );
    };
    let storage = match candidate_storage(
        executor,
        policy,
        &model,
        artifacts,
        &artifact_fingerprint,
        &candidate_name,
    )
    .await
    {
        Ok(storage) => storage,
        Err(_) => return executor_unavailable(),
    };
    let required_disk = required_disk_growth(&resources, &storage);
    let mut report = evaluate_admission(
        &snapshot.resource_policy,
        &snapshot.resources,
        &AdmissionRequest {
            desired,
            candidate: CandidateEnvelope::new(
                candidate_name,
                startup_peak,
                startup_peak,
                required_disk,
            ),
            compatibility_verified: true,
            guard_healthy: snapshot.health.guard_heartbeat,
        },
        unix_millis(),
    );
    report.selection = Some(super::resources::AdmissionSelection {
        engine_id: policy.config().id.clone(),
        selection_kind: "configured_engine".into(),
        engine: policy.config().family.clone(),
        image: policy.image(),
        fingerprint: policy.fingerprint().into(),
        artifacts: artifacts.clone(),
        artifact_fingerprint,
        compile_cache_namespace: storage.compile_cache_namespace,
    });
    Json(report).into_response()
}

#[utoipa::path(post, path = "/api/sy.spark/v1/instances", request_body = ServeRequest, responses((status = 202, body = OperationDocument)))]
async fn serve_instance(
    State(state): State<AgentState>,
    headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedToken>,
    Json(body): Json<ServeRequest>,
) -> Response {
    if body.dry_run {
        return admission(
            State(state),
            Json(ServeAdmissionRequest {
                model: body.model,
                name: body.name,
                dry_run: true,
            }),
        )
        .await;
    }
    if let Err(response) = require_executor_for_mutation(&state).await {
        return response;
    }
    let Some(database) = state.database.clone() else {
        return database_unavailable();
    };
    let Some(executor) = state.executor.clone() else {
        return executor_unavailable();
    };
    let Some(catalog) = state.engine_catalog.clone() else {
        return executor_unavailable();
    };
    let model = match database.model(&body.model).await {
        Ok(model) => model,
        Err(error) => return state_problem(error),
    };
    let Some(artifacts) = model.artifacts.as_ref() else {
        return problem(
            StatusCode::CONFLICT,
            "spark.engine.artifacts-required",
            "model has no verified artifact traits for engine selection",
        );
    };
    let policy = match catalog.select(artifacts) {
        Ok(policy) => policy,
        Err(error) => return problem(StatusCode::CONFLICT, "spark.engine.unsupported", &error),
    };
    let artifact_fingerprint = match super::engine::artifact_fingerprint(artifacts) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            return problem(
                StatusCode::CONFLICT,
                "spark.engine.artifacts-invalid",
                &error,
            )
        }
    };
    let resources = match policy.resources_for(None, artifacts) {
        Ok(resources) => resources,
        Err(error) => return problem(StatusCode::CONFLICT, "spark.engine.profile-invalid", &error),
    };
    let profile = match policy.profile_for(None, artifacts) {
        Ok(profile) => profile,
        Err(error) => return problem(StatusCode::CONFLICT, "spark.engine.profile-invalid", &error),
    };
    let context_window = profile.context_window;
    let default_reasoning_effort = profile.sampling.default_reasoning_effort.clone();
    let snapshot = match executor.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(_) => return executor_unavailable(),
    };
    let desired = match database.desired_resource_envelopes().await {
        Ok(envelopes) => envelopes,
        Err(error) => return state_problem(error),
    };
    let name = match normalize_instance_name(
        body.name
            .as_deref()
            .or_else(|| model.aliases.first().map(String::as_str))
            .unwrap_or(&model.id),
    ) {
        Some(name) => name,
        None => {
            return problem(
                StatusCode::BAD_REQUEST,
                "spark.instance.invalid-name",
                "instance name is invalid",
            );
        }
    };
    let storage = match candidate_storage(
        &executor,
        policy,
        &model,
        artifacts,
        &artifact_fingerprint,
        &name,
    )
    .await
    {
        Ok(storage) => storage,
        Err(_) => return executor_unavailable(),
    };
    let required_disk = required_disk_growth(&resources, &storage);
    let report = evaluate_admission(
        &snapshot.resource_policy,
        &snapshot.resources,
        &AdmissionRequest {
            desired,
            candidate: CandidateEnvelope::new(
                name.clone(),
                resources.startup_peak_bytes,
                resources.startup_peak_bytes,
                required_disk,
            ),
            compatibility_verified: true,
            guard_healthy: snapshot.health.guard_heartbeat,
        },
        unix_millis(),
    );
    if !report.admitted {
        return problem(
            StatusCode::CONFLICT,
            "spark.memory.admission-rejected",
            "aggregate resource admission rejected the instance",
        );
    }
    let transition = match state.admission.try_acquire(format!("serve:{name}")) {
        Ok(lease) => lease,
        Err(TransitionLeaseError::Busy { .. }) => {
            return problem(
                StatusCode::CONFLICT,
                "spark.memory.transition-busy",
                "another high-memory transition owns the admission lease",
            );
        }
        Err(TransitionLeaseError::Unavailable) => {
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "spark.memory.admission-unavailable",
                "the admission coordinator is unavailable",
            );
        }
    };
    let key = match required_idempotency(&headers) {
        Ok(key) => key,
        Err(()) => return missing_idempotency(),
    };
    let request_hash = match super::wire::canonical_request_sha256(&body) {
        Ok(hash) => hash,
        Err(_) => {
            return problem(
                StatusCode::BAD_REQUEST,
                "spark.request.invalid",
                "serve request cannot be canonicalized",
            );
        }
    };
    let accepted = match database
        .accept_operation(
            &auth.id,
            "instance.serve",
            &key,
            &request_hash,
            Some(name.clone()),
        )
        .await
    {
        Ok(accepted) => accepted,
        Err(error) => return state_problem(error),
    };
    if accepted.reused {
        return accepted_response(accepted.operation);
    }
    let instance = InstanceDocument {
        schema: INSTANCE_SCHEMA.into(),
        id: instance_id(&name),
        name,
        model_id: model.id.clone(),
        model: model.canonical.clone(),
        model_commit: model.commit.clone(),
        engine_id: policy.config().id.clone(),
        engine_fingerprint: policy.fingerprint().into(),
        artifacts: artifacts.clone(),
        artifact_fingerprint,
        objective: "inference".into(),
        resources,
        context_window,
        default_reasoning_effort,
        generation: 0,
        desired: InstanceDesiredState::Running,
        observed: InstanceObservedState::Creating,
        endpoint: None,
        healthy: false,
        started_at: None,
        startup_milliseconds: None,
        last_failure: None,
        restart_failures: 0,
        restart_suppressed: false,
        quarantine: None,
    };
    let begun = match database.begin_serve(instance).await {
        Ok(begun) => begun,
        Err(error) => return state_problem(error),
    };
    if begun.reused {
        complete_reused_serve(&database, &accepted.operation.id, &begun.instance).await;
    } else {
        state
            .routes
            .mark_warming(&begun.instance.name, begun.instance.generation);
        let slots = Arc::clone(&state.start_slots);
        let routes = state.routes.clone();
        let operation_id = accepted.operation.id.clone();
        tokio::spawn(run_serve(
            database,
            executor,
            slots,
            routes,
            transition,
            operation_id,
            begun.instance,
        ));
    }
    accepted_response(accepted.operation)
}

async fn complete_reused_serve(
    database: &DbActor,
    operation_id: &str,
    instance: &InstanceDocument,
) {
    let _ = database
        .transition(
            operation_id,
            super::wire::OperationState::Running,
            lifecycle_progress("existing", "matching instance already exists"),
            None,
            None,
        )
        .await;
    let _ = database
        .transition(
            operation_id,
            super::wire::OperationState::Succeeded,
            lifecycle_progress("complete", "matching instance reused"),
            serde_json::to_value(instance).ok(),
            None,
        )
        .await;
}

async fn run_serve(
    database: DbActor,
    executor: ExecutorClient,
    slots: Arc<tokio::sync::Semaphore>,
    routes: RouteRegistry,
    transition: TransitionLease,
    operation_id: String,
    instance: InstanceDocument,
) {
    let Ok(_permit) = slots.acquire_owned().await else {
        fail_serve(
            &database,
            &executor,
            &operation_id,
            &instance,
            &routes,
            "start queue unavailable",
        )
        .await;
        return;
    };
    if database
        .transition(
            &operation_id,
            super::wire::OperationState::Running,
            lifecycle_progress(
                "pulling-image",
                "ensuring exact digest image and managed network",
            ),
            None,
            None,
        )
        .await
        .is_err()
    {
        return;
    }
    if serve_cancelled(&database, &operation_id).await {
        cancel_prehealth_serve(&database, &executor, &instance).await;
        return;
    }
    let prepared = match executor
        .prepare_instance(StartInstanceInput {
            instance_id: instance.id.clone(),
            generation: instance.generation,
            model_commit: instance.model_commit.clone(),
            model_repository: instance
                .model
                .strip_prefix("huggingface:")
                .and_then(|model| model.split_once('@'))
                .map(|(repository, _)| repository)
                .unwrap_or_default()
                .to_owned(),
            engine_id: instance.engine_id.clone(),
            engine_fingerprint: instance.engine_fingerprint.clone(),
            artifacts: instance.artifacts.clone(),
            artifact_fingerprint: instance.artifact_fingerprint.clone(),
            operation_id: operation_id.clone(),
        })
        .await
    {
        Ok(prepared) => prepared,
        Err(_) => {
            fail_serve(
                &database,
                &executor,
                &operation_id,
                &instance,
                &routes,
                "exact image or network preparation failed",
            )
            .await;
            return;
        }
    };
    if serve_cancelled(&database, &operation_id).await {
        cancel_prehealth_serve(&database, &executor, &instance).await;
        return;
    }
    if database
        .transition(
            &operation_id,
            super::wire::OperationState::Running,
            lifecycle_progress("creating", "creating isolated engine generation"),
            None,
            None,
        )
        .await
        .is_err()
    {
        return;
    }
    let observed = match executor.start_prepared(prepared).await {
        Ok(observed) => observed,
        Err(_) => {
            fail_serve(
                &database,
                &executor,
                &operation_id,
                &instance,
                &routes,
                "engine creation failed",
            )
            .await;
            return;
        }
    };
    // Pulling an exact image is preparation, not engine startup. A cold pull must
    // not consume the configured health and semantic-readiness deadline.
    let startup_started = tokio::time::Instant::now();
    if serve_cancelled(&database, &operation_id).await {
        cancel_prehealth_serve(&database, &executor, &instance).await;
        return;
    }
    let address = match observed.address.parse() {
        Ok(address) => address,
        Err(_) => {
            fail_serve(
                &database,
                &executor,
                &operation_id,
                &instance,
                &routes,
                "executor returned an invalid bridge address",
            )
            .await;
            return;
        }
    };
    let allowed = observed
        .allowed_routes
        .iter()
        .map(|(method, path)| (method.as_str(), path.as_str()));
    let health = ObservedRoute::new(
        &observed.instance_id,
        observed.generation,
        address,
        observed.port,
        allowed,
    )
    .and_then(|route| {
        if route.identity() != (observed.instance_id.as_str(), observed.generation) {
            return Err(super::upstream::UpstreamError::identity_mismatch());
        }
        route
            .request(&observed.health_method, &observed.health_path, 0)
            .map(|request| (route, request))
    });
    let readiness_interrupt = ReadinessInterrupt::new(
        database.clone(),
        operation_id.clone(),
        instance.id.clone(),
        executor.clone(),
        instance.generation,
    );
    let ready_route = match health {
        Ok((route, request)) => {
            let deadline = Duration::from_secs(observed.startup_deadline_seconds);
            let remaining = deadline.saturating_sub(startup_started.elapsed());
            wait_until_engine_ready(
                &route,
                &request,
                observed.health_body.as_ref(),
                remaining,
                Some(&readiness_interrupt),
            )
            .await
            .then_some(route)
        }
        Err(_) => None,
    };
    if readiness_interrupt.instance_stopped().await {
        return;
    }
    if readiness_interrupt.operation_cancelled().await {
        cancel_prehealth_serve(&database, &executor, &instance).await;
        return;
    }
    let semantic_failure = match ready_route.as_ref() {
        Some(route) => {
            let failure = semantic_probe_result(
                route,
                &observed,
                semantic_probe_timeout(
                    Duration::from_secs(observed.startup_deadline_seconds),
                    startup_started.elapsed(),
                ),
            )
            .await
            .err();
            if let Some(reason) = failure {
                tracing::error!(
                    category = "semantic-contract-rejected",
                    instance_id = %instance.id,
                    generation = instance.generation,
                    reason,
                    "Spark engine semantic readiness failed"
                );
            }
            failure
        }
        None => None,
    };
    if ready_route.is_none() || semantic_failure.is_some() {
        routes.drain(&instance.name, instance.generation);
        let detail = semantic_failure.map_or_else(
            || "engine health check failed".to_owned(),
            semantic_failure_detail,
        );
        fail_serve(
            &database,
            &executor,
            &operation_id,
            &instance,
            &routes,
            &detail,
        )
        .await;
        return;
    }
    if serve_cancelled(&database, &operation_id).await {
        cancel_prehealth_serve(&database, &executor, &instance).await;
        return;
    }
    if executor
        .promote_restart(StopInstanceInput {
            instance_id: instance.id.clone(),
            generation: instance.generation,
            grace_seconds: 0,
        })
        .await
        .is_err()
    {
        fail_serve(
            &database,
            &executor,
            &operation_id,
            &instance,
            &routes,
            "restart policy promotion failed",
        )
        .await;
        return;
    }
    let endpoint = format!("/openai/{}/v1", instance.name);
    let healthy_instance = match database
        .set_instance_observed(
            &instance.id,
            instance.generation,
            InstanceObservedState::Healthy,
            Some(endpoint),
            None,
            Some(u64::try_from(startup_started.elapsed().as_millis()).unwrap_or(u64::MAX)),
        )
        .await
    {
        Ok(instance) => instance,
        Err(_) => {
            fail_serve(
                &database,
                &executor,
                &operation_id,
                &instance,
                &routes,
                "healthy state persistence failed",
            )
            .await;
            return;
        }
    };
    if let Some(route) = ready_route {
        routes.publish_with_profile(
            &instance.name,
            instance.model.clone(),
            observed.served_model,
            observed.gateway_profile,
            route,
        );
    }
    let _ = database
        .transition(
            &operation_id,
            super::wire::OperationState::Succeeded,
            lifecycle_progress("complete", "engine is healthy and durable"),
            serde_json::to_value(healthy_instance).ok(),
            None,
        )
        .await;
    let _ = reconcile_once(
        &database,
        &executor,
        Some(&routes),
        &transition.coordinator(),
    )
    .await;
    drop(transition);
}

async fn serve_cancelled(database: &DbActor, operation_id: &str) -> bool {
    database
        .operation(operation_id)
        .await
        .is_ok_and(|operation| operation.state == super::wire::OperationState::Cancelled)
}

async fn cancel_prehealth_serve(
    database: &DbActor,
    executor: &ExecutorClient,
    instance: &InstanceDocument,
) {
    let _ = database.begin_stop(&instance.id).await;
    let _ = executor
        .stop_instance(StopInstanceInput {
            instance_id: instance.id.clone(),
            generation: instance.generation,
            grace_seconds: 5,
        })
        .await;
    let _ = database
        .set_instance_observed(
            &instance.id,
            instance.generation,
            InstanceObservedState::Absent,
            None,
            None,
            None,
        )
        .await;
}

struct ReadinessInterrupt {
    database: DbActor,
    operation_id: String,
    instance_id: String,
    executor: ExecutorClient,
    generation: u64,
}

impl ReadinessInterrupt {
    fn new(
        database: DbActor,
        operation_id: String,
        instance_id: String,
        executor: ExecutorClient,
        generation: u64,
    ) -> Self {
        Self {
            database,
            operation_id,
            instance_id,
            executor,
            generation,
        }
    }

    async fn operation_cancelled(&self) -> bool {
        serve_cancelled(&self.database, &self.operation_id).await
    }

    async fn instance_stopped(&self) -> bool {
        self.database
            .instance(&self.instance_id)
            .await
            .is_ok_and(|instance| instance.desired == InstanceDesiredState::Stopped)
    }

    async fn triggered(&self) -> bool {
        self.operation_cancelled().await
            || self.instance_stopped().await
            || self
                .executor
                .inspect_instance(StopInstanceInput {
                    instance_id: self.instance_id.clone(),
                    generation: self.generation,
                    grace_seconds: 0,
                })
                .await
                .is_ok_and(|running| running != Some(true))
    }
}

async fn wait_until_engine_ready(
    route: &ObservedRoute,
    request: &super::upstream::UpstreamRequest,
    health_body: Option<&super::engine::EngineHealthBody>,
    timeout: Duration,
    interrupt: Option<&ReadinessInterrupt>,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(interrupt) = interrupt {
            if interrupt.triggered().await {
                return false;
            }
        }
        if route
            .send(request, &[])
            .await
            .is_ok_and(|response| health_response_is_ready(&response, health_body))
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(ENGINE_READINESS_INTERVAL).await;
    }
}

fn health_response_is_ready(
    response: &super::upstream::UpstreamResponse,
    health_body: Option<&super::engine::EngineHealthBody>,
) -> bool {
    if !(200..300).contains(&response.status) {
        return false;
    }
    let Some(rule) = health_body else {
        return true;
    };
    serde_json::from_slice::<serde_json::Value>(&response.bytes)
        .ok()
        .and_then(|body| body.pointer(&rule.json_pointer).cloned())
        .and_then(|value| value.as_str().map(str::to_owned))
        .is_some_and(|value| value == rule.equals)
}

fn semantic_probe_timeout(startup_deadline: Duration, elapsed: Duration) -> Duration {
    startup_deadline
        .saturating_sub(elapsed)
        .min(super::upstream::MAX_SEMANTIC_PROBE_TIMEOUT)
}

async fn reconcile_once(
    database: &DbActor,
    executor: &ExecutorClient,
    routes: Option<&RouteRegistry>,
    coordinator: &TransitionCoordinator,
) -> Result<(), ()> {
    let instances = database.list_instances().await.map_err(|_| ())?;
    let expected = instances
        .iter()
        .filter(|instance| reconcile_expects_container(instance))
        .map(|instance| ReconcileExpectation {
            instance_id: instance.id.clone(),
            generation: instance.generation,
            model_commit: instance.model_commit.clone(),
            model_repository: instance
                .model
                .strip_prefix("huggingface:")
                .and_then(|model| model.split_once('@'))
                .map(|(repository, _)| repository)
                .unwrap_or_default()
                .to_owned(),
            engine_id: instance.engine_id.clone(),
            engine_fingerprint: instance.engine_fingerprint.clone(),
            artifacts: instance.artifacts.clone(),
            artifact_fingerprint: instance.artifact_fingerprint.clone(),
        })
        .collect();
    let scan = executor.reconcile_scan(expected).await.map_err(|_| ())?;
    let quarantined = scan
        .quarantined
        .into_iter()
        .map(|quarantine| QuarantineEvidence {
            container_id: quarantine.container_id,
            instance_id: quarantine.instance_id,
            generation: quarantine.generation,
            cause: quarantine.cause,
        })
        .collect::<Vec<_>>();
    database
        .reconcile_quarantine(quarantined.clone())
        .await
        .map_err(|_| ())?;
    for quarantine in quarantined {
        let Ok(_transition) =
            coordinator.try_acquire(format!("reconcile-quarantine:{}", quarantine.container_id))
        else {
            continue;
        };
        if let Some((instance_id, generation)) = quarantine.instance_id.zip(quarantine.generation) {
            let _ = database
                .mark_quarantine(&instance_id, generation, &quarantine.cause)
                .await;
        }
    }
    let matched = scan
        .matched
        .into_iter()
        .map(|engine| ((engine.instance_id.clone(), engine.generation), engine))
        .collect::<std::collections::BTreeMap<_, _>>();
    let operations = database.list_operations().await.map_err(|_| ())?;
    for instance in instances {
        let Ok(_transition) =
            coordinator.try_acquire(format!("reconcile:{}:{}", instance.id, instance.generation))
        else {
            continue;
        };
        let identity = (instance.id.clone(), instance.generation);
        let observed = matched.get(&identity).cloned();
        let desired = if instance.desired == InstanceDesiredState::Running {
            DesiredIntent::Running
        } else {
            DesiredIntent::Stopped
        };
        let validated = match observed.as_ref() {
            Some(engine) if engine.running => ValidatedObservation::ExactHealthy,
            Some(_) => ValidatedObservation::ExactUnhealthy,
            None => ValidatedObservation::Missing,
        };
        let action = decide(desired, validated, instance.restart_suppressed);
        if matches!(
            action,
            ReconcileAction::StopExact | ReconcileAction::MarkAbsent
        ) {
            if action == ReconcileAction::StopExact {
                executor
                    .stop_instance(StopInstanceInput {
                        instance_id: instance.id.clone(),
                        generation: instance.generation,
                        grace_seconds: 5,
                    })
                    .await
                    .map_err(|_| ())?;
            }
            database
                .set_instance_observed(
                    &instance.id,
                    instance.generation,
                    InstanceObservedState::Absent,
                    None,
                    None,
                    None,
                )
                .await
                .map_err(|_| ())?;
            if let Some(routes) = routes {
                routes.drain(&instance.name, instance.generation);
            }
            complete_recovered_operation(database, &operations, &instance, routes, "instance.stop")
                .await;
            continue;
        }
        if action == ReconcileAction::KeepSuppressed {
            continue;
        }
        if let Some(observed) = observed {
            reconcile_running_engine(database, executor, routes, &instance, observed).await?;
            complete_recovered_operation(
                database,
                &operations,
                &instance,
                routes,
                "instance.serve",
            )
            .await;
            continue;
        }
        let failed = database
            .record_restart_failure(&instance.id, instance.generation, unix_millis() / 1_000)
            .await
            .map_err(|_| ())?;
        if failed.restart_suppressed || failed.quarantine.is_some() {
            continue;
        }
        let Some(operation_id) = operations
            .iter()
            .find(|operation| {
                operation.kind == "instance.serve"
                    && operation.target.as_deref() == Some(instance.name.as_str())
            })
            .map(|operation| operation.id.clone())
        else {
            continue;
        };
        if !persistent_restart_allowed(database, executor).await? {
            continue;
        }
        let Ok(prepared) = executor
            .prepare_instance(StartInstanceInput {
                instance_id: instance.id.clone(),
                generation: instance.generation,
                model_commit: instance.model_commit.clone(),
                model_repository: instance
                    .model
                    .strip_prefix("huggingface:")
                    .and_then(|model| model.split_once('@'))
                    .map(|(repository, _)| repository)
                    .unwrap_or_default()
                    .to_owned(),
                engine_id: instance.engine_id.clone(),
                engine_fingerprint: instance.engine_fingerprint.clone(),
                artifacts: instance.artifacts.clone(),
                artifact_fingerprint: instance.artifact_fingerprint.clone(),
                operation_id,
            })
            .await
        else {
            continue;
        };
        let Ok(observed) = executor.start_prepared(prepared).await else {
            continue;
        };
        reconcile_running_engine(database, executor, routes, &failed, observed).await?;
        complete_recovered_operation(database, &operations, &instance, routes, "instance.serve")
            .await;
    }
    Ok(())
}

fn reconcile_expects_container(instance: &InstanceDocument) -> bool {
    instance.desired == InstanceDesiredState::Running
}

async fn complete_recovered_operation(
    database: &DbActor,
    operations: &[OperationDocument],
    instance: &InstanceDocument,
    routes: Option<&RouteRegistry>,
    kind: &str,
) {
    let target = if kind == "instance.stop" {
        instance.id.as_str()
    } else {
        instance.name.as_str()
    };
    let Some(operation) = operations.iter().find(|operation| {
        operation.kind == kind
            && operation.target.as_deref() == Some(target)
            && !operation.state.is_terminal()
    }) else {
        return;
    };
    let Ok(current) = database.instance(&instance.id).await else {
        return;
    };
    let route_ready = routes.is_some_and(|routes| {
        matches!(routes.lookup(&current.name), RouteLookup::Healthy(route) if route.generation == current.generation)
    });
    let route_absent =
        routes.is_none_or(|routes| matches!(routes.lookup(&current.name), RouteLookup::Missing));
    let recovered = match kind {
        "instance.serve" => {
            current.desired == InstanceDesiredState::Running
                && current.observed == InstanceObservedState::Healthy
                && current.healthy
                && current.endpoint.is_some()
                && route_ready
        }
        "instance.stop" => {
            current.desired == InstanceDesiredState::Stopped
                && current.observed == InstanceObservedState::Absent
                && !current.healthy
                && current.endpoint.is_none()
                && route_absent
        }
        _ => false,
    };
    if !recovered {
        return;
    }
    let result = serde_json::to_value(current).ok();
    if operation.state == super::wire::OperationState::Accepted {
        let _ = database
            .transition(
                &operation.id,
                super::wire::OperationState::Running,
                lifecycle_progress("recovering", "resuming durable operation"),
                None,
                None,
            )
            .await;
    }
    let _ = database
        .transition(
            &operation.id,
            super::wire::OperationState::Succeeded,
            lifecycle_progress("complete", "operation recovered by exact reconciliation"),
            result,
            None,
        )
        .await;
}

async fn persistent_restart_allowed(
    database: &DbActor,
    executor: &ExecutorClient,
) -> Result<bool, ()> {
    let snapshot = executor.snapshot().await.map_err(|_| ())?;
    let desired = database
        .desired_resource_envelopes()
        .await
        .map_err(|_| ())?;
    Ok(snapshot.health.guard_heartbeat
        && persistent_set_fits_reboot_envelope(
            &snapshot.resource_policy,
            &snapshot.resources,
            &desired,
            unix_millis(),
        ))
}

async fn reconcile_running_engine(
    database: &DbActor,
    executor: &ExecutorClient,
    routes: Option<&RouteRegistry>,
    instance: &InstanceDocument,
    observed: ObservedEngine,
) -> Result<(), ()> {
    if observed.running
        && observed.restart_policy == "unless-stopped"
        && routes.is_some_and(|routes| exact_route_is_published(routes, instance))
    {
        return Ok(());
    }
    if !observed.running || !persistent_restart_allowed(database, executor).await? {
        let _ = executor
            .disable_restart(StopInstanceInput {
                instance_id: instance.id.clone(),
                generation: instance.generation,
                grace_seconds: 0,
            })
            .await;
        database
            .record_restart_failure(&instance.id, instance.generation, unix_millis() / 1_000)
            .await
            .map_err(|_| ())?;
        return Ok(());
    }
    let address = observed.address.parse().map_err(|_| ())?;
    let route = ObservedRoute::new(
        &observed.instance_id,
        observed.generation,
        address,
        observed.port,
        observed
            .allowed_routes
            .iter()
            .map(|(method, path)| (method.as_str(), path.as_str())),
    )
    .map_err(|_| ())?;
    let request = route
        .request(&observed.health_method, &observed.health_path, 0)
        .map_err(|_| ())?;
    let deadline = Duration::from_secs(observed.startup_deadline_seconds);
    let readiness_started = tokio::time::Instant::now();
    if !wait_until_engine_ready(
        &route,
        &request,
        observed.health_body.as_ref(),
        deadline,
        None,
    )
    .await
        || semantic_probe_result(
            &route,
            &observed,
            semantic_probe_timeout(deadline, readiness_started.elapsed()),
        )
        .await
        .is_err()
    {
        let _ = executor
            .disable_restart(StopInstanceInput {
                instance_id: instance.id.clone(),
                generation: instance.generation,
                grace_seconds: 0,
            })
            .await;
        database
            .record_restart_failure(&instance.id, instance.generation, unix_millis() / 1_000)
            .await
            .map_err(|_| ())?;
        return Ok(());
    }
    if observed.restart_policy != "unless-stopped" {
        executor
            .promote_restart(StopInstanceInput {
                instance_id: instance.id.clone(),
                generation: instance.generation,
                grace_seconds: 0,
            })
            .await
            .map_err(|_| ())?;
    }
    let endpoint = format!("/openai/{}/v1", instance.name);
    database
        .set_instance_observed(
            &instance.id,
            instance.generation,
            InstanceObservedState::Healthy,
            Some(endpoint),
            None,
            None,
        )
        .await
        .map(|_| {
            if let Some(routes) = routes {
                routes.publish_with_profile(
                    &instance.name,
                    instance.model.clone(),
                    observed.served_model,
                    observed.gateway_profile,
                    route,
                );
            }
        })
        .map_err(|_| ())
}

fn exact_route_is_published(routes: &RouteRegistry, instance: &InstanceDocument) -> bool {
    matches!(
        routes.lookup(&instance.name),
        RouteLookup::Healthy(route) if route.generation == instance.generation
    )
}

async fn semantic_probe_result(
    route: &ObservedRoute,
    observed: &ObservedEngine,
    timeout: Duration,
) -> Result<(), &'static str> {
    let started = tokio::time::Instant::now();
    if let Some(policy) = &observed.gateway_profile.embeddings {
        return route
            .embedding_probe(
                &observed.served_model,
                &observed.semantic_prompt,
                policy.dimensions,
                policy.normalized,
                policy.normalization_tolerance_ppm,
                timeout,
            )
            .await
            .map_err(|error| error.diagnostic());
    } else if let Some(policy) = &observed.gateway_profile.vision {
        let image = gateway::vision_health_image(policy).map_err(|error| error.message)?;
        route
            .vision_probe(
                VisionProbe {
                    served_model: &observed.served_model,
                    prompt: &policy.health_prompt,
                    image: &image,
                    expected_text: &policy.health_expected_text,
                    max_tokens: policy.health_max_tokens,
                    disable_thinking: policy.health_disable_thinking,
                },
                timeout,
            )
            .await
            .map_err(|error| error.diagnostic())?;
    } else {
        route
            .semantic_probe(
                &observed.served_model,
                &observed.semantic_prompt,
                observed.semantic_max_tokens,
                timeout,
            )
            .await
            .map_err(|error| error.diagnostic())?;
    }
    if !observed.gateway_profile.startup_protocol_probe {
        return Ok(());
    }
    let remaining = timeout.saturating_sub(started.elapsed());
    let require_tools = observed
        .gateway_profile
        .capabilities
        .contains("tool_calling");
    route
        .protocol_probe(&observed.served_model, require_tools, remaining)
        .await
        .map_err(|error| error.diagnostic())
}

fn semantic_failure_detail(reason: &'static str) -> String {
    format!("engine semantic capability contract failed: {reason}")
}

async fn fail_serve(
    database: &DbActor,
    executor: &ExecutorClient,
    operation_id: &str,
    instance: &InstanceDocument,
    routes: &RouteRegistry,
    detail: &str,
) {
    routes.drain(&instance.name, instance.generation);
    let failure_detail = executor
        .logs(LogInput {
            instance_id: instance.id.clone(),
            generation: instance.generation,
            cursor: 0,
            limit: 12,
        })
        .await
        .map_or_else(
            |_| detail.to_owned(),
            |logs| failure_detail_with_logs(detail, &logs.lines),
        );
    let _ = executor
        .stop_instance(StopInstanceInput {
            instance_id: instance.id.clone(),
            generation: instance.generation,
            grace_seconds: 5,
        })
        .await;
    let _ = database.begin_stop(&instance.id).await;
    let _ = database
        .set_instance_observed(
            &instance.id,
            instance.generation,
            InstanceObservedState::Failed,
            None,
            Some(failure_detail.clone()),
            None,
        )
        .await;
    let problem = ProblemDocument {
        schema: PROBLEM_SCHEMA.into(),
        r#type: "https://sy.local/problems/spark-engine-start".into(),
        code: "spark.instance.start-failed".into(),
        status: 503,
        detail: failure_detail.clone(),
        remediation: vec![
            "inspect the instance last_failure diagnostic before serving again".into(),
        ],
        operation_id: Some(operation_id.into()),
    };
    let _ = database
        .transition(
            operation_id,
            super::wire::OperationState::Failed,
            lifecycle_progress("failed", &failure_detail),
            None,
            Some(problem),
        )
        .await;
}

fn failure_detail_with_logs(detail: &str, lines: &[String]) -> String {
    const MAX_FAILURE_DETAIL_CHARS: usize = 2_048;
    let mut result = detail.to_owned();
    if !lines.is_empty() {
        result.push_str("; final engine log: ");
        result.push_str(&lines.join(" | "));
    }
    result.chars().take(MAX_FAILURE_DETAIL_CHARS).collect()
}

#[utoipa::path(get, path = "/api/sy.spark/v1/instances", responses((status = 200, body = InstanceListDocument)))]
async fn list_instances(State(state): State<AgentState>, RawQuery(query): RawQuery) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    match database.list_instances().await {
        Ok(mut instances) => {
            project_route_health(&state.routes, &mut instances);
            Json(InstanceListDocument {
                schema: INSTANCE_LIST_SCHEMA.into(),
                instances,
            })
            .into_response()
        }
        Err(error) => state_problem(error),
    }
}

fn project_route_health(routes: &RouteRegistry, instances: &mut [InstanceDocument]) {
    for instance in instances.iter_mut().filter(|instance| instance.healthy) {
        if !matches!(routes.lookup(&instance.name), RouteLookup::Healthy(route) if route.generation == instance.generation)
        {
            instance.observed = InstanceObservedState::Degraded;
            instance.healthy = false;
            instance.endpoint = None;
        }
    }
}

#[utoipa::path(delete, path = "/api/sy.spark/v1/instances/{id}", params(("id" = String, Path)), request_body = StopRequest, responses((status = 202, body = OperationDocument)))]
async fn stop_instance(
    State(state): State<AgentState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedToken>,
    Json(body): Json<StopRequest>,
) -> Response {
    if body.timeout_seconds > 300 {
        return problem(
            StatusCode::BAD_REQUEST,
            "spark.instance.invalid-timeout",
            "stop timeout must be at most 300 seconds",
        );
    }
    let Some(database) = state.database.clone() else {
        return database_unavailable();
    };
    let instance = match database.instance(&id).await {
        Ok(instance) => instance,
        Err(error) => return state_problem(error),
    };
    if body.dry_run {
        return Json(instance).into_response();
    }
    if let Err(response) = require_executor_for_mutation(&state).await {
        return response;
    }
    let key = match required_idempotency(&headers) {
        Ok(key) => key,
        Err(()) => return missing_idempotency(),
    };
    let hash = match super::wire::canonical_request_sha256(&body) {
        Ok(hash) => hash,
        Err(_) => {
            return problem(
                StatusCode::BAD_REQUEST,
                "spark.request.invalid",
                "stop request cannot be canonicalized",
            );
        }
    };
    let Some(executor) = state.executor.clone() else {
        return executor_unavailable();
    };
    let accepted = match database
        .accept_operation(
            &auth.id,
            "instance.stop",
            &key,
            &hash,
            Some(instance.id.clone()),
        )
        .await
    {
        Ok(accepted) => accepted,
        Err(error) => return state_problem(error),
    };
    if !accepted.reused {
        let stopping = match database.begin_stop(&instance.id).await {
            Ok(instance) => instance,
            Err(error) => return state_problem(error),
        };
        let operation_id = accepted.operation.id.clone();
        let routes = state.routes.clone();
        if stopping.observed == InstanceObservedState::Absent && stopping.quarantine.is_none() {
            routes.drain(&stopping.name, stopping.generation);
            complete_absent_stop(&database, &operation_id, &stopping).await;
        } else {
            tokio::spawn(run_stop(
                database,
                executor,
                routes,
                operation_id,
                stopping,
                body.timeout_seconds,
            ));
        }
    }
    accepted_response(accepted.operation)
}

async fn run_stop(
    database: DbActor,
    executor: ExecutorClient,
    routes: RouteRegistry,
    operation_id: String,
    instance: InstanceDocument,
    timeout_seconds: u64,
) {
    routes.drain(&instance.name, instance.generation);
    let _ = database
        .transition(
            &operation_id,
            super::wire::OperationState::Running,
            lifecycle_progress("stopping", "restart disabled; draining exact generation"),
            None,
            None,
        )
        .await;
    let input = StopInstanceInput {
        instance_id: instance.id.clone(),
        generation: instance.generation,
        grace_seconds: timeout_seconds,
    };
    let stopped = stop_exact_generation(&executor, input, timeout_seconds).await;
    match stopped {
        true => {
            let stopped = database
                .set_instance_observed(
                    &instance.id,
                    instance.generation,
                    InstanceObservedState::Absent,
                    None,
                    None,
                    None,
                )
                .await
                .ok();
            let _ = database
                .transition(
                    &operation_id,
                    super::wire::OperationState::Succeeded,
                    lifecycle_progress("complete", "instance stopped; model and cache retained"),
                    stopped.and_then(|value| serde_json::to_value(value).ok()),
                    None,
                )
                .await;
        }
        false => {
            let problem = ProblemDocument {
                schema: PROBLEM_SCHEMA.into(),
                r#type: "https://sy.local/problems/spark-engine-stop".into(),
                code: "spark.instance.stop-failed".into(),
                status: 503,
                detail: "exact managed generation could not be stopped".into(),
                remediation: vec!["retry stop with the same instance identity".into()],
                operation_id: Some(operation_id.clone()),
            };
            let _ = database
                .transition(
                    &operation_id,
                    super::wire::OperationState::Failed,
                    lifecycle_progress("failed", &problem.detail),
                    None,
                    Some(problem),
                )
                .await;
        }
    }
}

async fn stop_exact_generation(
    executor: &ExecutorClient,
    input: StopInstanceInput,
    timeout_seconds: u64,
) -> bool {
    if executor.stop_instance(input.clone()).await.is_ok() {
        return true;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        match executor.inspect_instance(input.clone()).await {
            Ok(None) => return true,
            Ok(Some(false)) => {
                return executor.stop_instance(input.clone()).await.is_ok()
                    || matches!(executor.inspect_instance(input).await, Ok(None));
            }
            Ok(Some(true)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(STOP_COMPLETION_POLL_INTERVAL).await;
            }
            _ => return false,
        }
    }
}

async fn complete_absent_stop(database: &DbActor, operation_id: &str, instance: &InstanceDocument) {
    let _ = database
        .transition(
            operation_id,
            super::wire::OperationState::Running,
            lifecycle_progress("stopping", "exact generation is already absent"),
            None,
            None,
        )
        .await;
    let _ = database
        .transition(
            operation_id,
            super::wire::OperationState::Succeeded,
            lifecycle_progress(
                "complete",
                "instance already stopped; model and cache retained",
            ),
            serde_json::to_value(instance).ok(),
            None,
        )
        .await;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogsQuery {
    #[serde(default)]
    cursor: u64,
    #[serde(default = "default_log_limit")]
    limit: usize,
}

fn default_log_limit() -> usize {
    100
}

#[utoipa::path(get, path = "/api/sy.spark/v1/instances/{id}/logs", params(("id" = String, Path), ("cursor" = Option<u64>, Query), ("limit" = Option<usize>, Query)), responses((status = 200, body = EngineLogDocument)))]
async fn instance_logs(
    State(state): State<AgentState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<LogsQuery>,
) -> Response {
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    let instance = match database.instance(&id).await {
        Ok(instance) => instance,
        Err(error) => return state_problem(error),
    };
    let Some(executor) = &state.executor else {
        return executor_unavailable();
    };
    match executor
        .logs(LogInput {
            instance_id: instance.id.clone(),
            generation: instance.generation,
            cursor: query.cursor,
            limit: query.limit,
        })
        .await
    {
        Ok(logs) => Json(EngineLogDocument {
            schema: ENGINE_LOG_SCHEMA.into(),
            instance_id: instance.id,
            generation: instance.generation,
            cursor: logs.cursor,
            next_cursor: logs.next_cursor,
            truncated: logs.truncated,
            lines: logs.lines,
        })
        .into_response(),
        Err(_) => executor_unavailable(),
    }
}

fn normalize_instance_name(value: &str) -> Option<String> {
    let name = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    (!name.is_empty() && name.len() <= 63 && !name.starts_with('-') && !name.ends_with('-'))
        .then_some(name)
}

fn instance_id(name: &str) -> String {
    format!("i_{:x}", sha2::Sha256::digest(name.as_bytes()))[..34].into()
}

fn required_disk_growth(
    resources: &super::wire::RecipeResourceEnvelopeDocument,
    storage: &CandidateStorage,
) -> u64 {
    let image = (!storage.image_present)
        .then_some(resources.image_bytes)
        .unwrap_or(0);
    let cache = resources
        .compile_cache_bytes
        .checked_sub(storage.compile_cache_allocated_bytes)
        .unwrap_or(resources.compile_cache_bytes);
    image.saturating_add(cache)
}

async fn candidate_storage(
    executor: &ExecutorClient,
    policy: &super::engine::EnginePolicy,
    model: &ModelDocument,
    artifacts: &super::wire::ModelArtifactsDocument,
    artifact_fingerprint: &str,
    name: &str,
) -> Result<CandidateStorage, super::executor::ExecutorClientError> {
    executor
        .candidate_storage(CandidateStorageInput {
            instance_id: instance_id(name),
            model_commit: model.commit.clone(),
            model_repository: model.repository.clone(),
            engine_id: policy.config().id.clone(),
            engine_fingerprint: policy.fingerprint().into(),
            artifacts: artifacts.clone(),
            artifact_fingerprint: artifact_fingerprint.into(),
        })
        .await
}

fn lifecycle_progress(stage: &str, message: &str) -> OperationProgress {
    OperationProgress {
        stage: stage.into(),
        current: None,
        total: None,
        unit: None,
        message: message.into(),
    }
}

fn executor_unavailable() -> Response {
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "spark.executor.unavailable",
        "the managed Spark executor operation is unavailable",
    )
}

#[utoipa::path(get, path = "/api/sy.spark/v1/models", responses((status = 200, body = ModelListDocument)))]
async fn list_models(State(state): State<AgentState>, RawQuery(query): RawQuery) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    match database.list_models().await {
        Ok(models) => Json(ModelListDocument {
            schema: MODEL_LIST_SCHEMA.into(),
            models,
        })
        .into_response(),
        Err(error) => state_problem(error),
    }
}

#[utoipa::path(get, path = "/api/sy.spark/v1/models/{id}", params(("id" = String, Path)), responses((status = 200, body = ModelDocument)))]
async fn get_model(
    State(state): State<AgentState>,
    AxumPath(id): AxumPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    if !valid_model_id(&id) {
        return problem(
            StatusCode::BAD_REQUEST,
            "spark.request.invalid-id",
            "model ID is invalid",
        );
    }
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    match database.model(&id).await {
        Ok(model) => Json(model).into_response(),
        Err(error) => state_problem(error),
    }
}

struct DownloadTarget {
    repository: Repository,
    revision: Revision,
    alias: Option<Alias>,
    selection: ArtifactSelection,
}

#[derive(Debug)]
struct DownloadTargetError {
    code: &'static str,
    detail: &'static str,
}

fn resolve_download_target(
    request: &DownloadRequest,
    catalog: Option<&super::model_catalog::ModelCatalog>,
) -> Result<DownloadTarget, DownloadTargetError> {
    if let Some(configured) = catalog.and_then(|catalog| catalog.resolve(&request.repository)) {
        if request.revision != "main"
            || request.alias.is_some()
            || request.artifact.is_some()
            || !request.auxiliary.is_empty()
        {
            return Err(DownloadTargetError {
                code: "spark.model.catalog-override",
                detail: "configured model aliases cannot override revision or artifacts",
            });
        }
        let selection =
            ArtifactSelection::configured(configured.artifacts().clone()).map_err(|_| {
                DownloadTargetError {
                    code: "spark.model.invalid-catalog",
                    detail: "configured model artifacts are invalid",
                }
            })?;
        let configured_alias = selection
            .configured_artifacts()
            .and_then(|artifacts| artifacts.configured_alias.as_deref())
            .ok_or(DownloadTargetError {
                code: "spark.model.invalid-catalog",
                detail: "configured model artifact alias is missing",
            })?;
        return Ok(DownloadTarget {
            repository: Repository::parse(configured.repository()).map_err(|_| {
                DownloadTargetError {
                    code: "spark.model.invalid-catalog",
                    detail: "configured model repository is invalid",
                }
            })?,
            revision: Revision::parse(configured.revision()).map_err(|_| DownloadTargetError {
                code: "spark.model.invalid-catalog",
                detail: "configured model revision is invalid",
            })?,
            alias: Some(
                Alias::parse(configured_alias).map_err(|_| DownloadTargetError {
                    code: "spark.model.invalid-catalog",
                    detail: "configured model alias is invalid",
                })?,
            ),
            selection,
        });
    }
    let repository = Repository::parse(&request.repository).map_err(|_| DownloadTargetError {
        code: "spark.model.invalid-repository",
        detail: "model repository is invalid",
    })?;
    let revision = Revision::parse(&request.revision).map_err(|_| DownloadTargetError {
        code: "spark.model.invalid-revision",
        detail: "model revision is invalid",
    })?;
    let alias = request
        .alias
        .as_deref()
        .map(Alias::parse)
        .transpose()
        .map_err(|_| DownloadTargetError {
            code: "spark.model.invalid-alias",
            detail: "model alias is invalid",
        })?;
    let selection =
        match request.artifact.as_deref() {
            Some(primary) => ArtifactSelection::generic(primary, request.auxiliary.clone())
                .map_err(|_| DownloadTargetError {
                    code: "spark.model.invalid-artifacts",
                    detail: "model artifact selectors are invalid or ambiguous",
                })?,
            None => ArtifactSelection::automatic(),
        };
    Ok(DownloadTarget {
        repository,
        revision,
        alias,
        selection,
    })
}

#[utoipa::path(post, path = "/api/sy.spark/v1/downloads", request_body = DownloadRequest, responses((status = 200, body = DownloadPlanDocument), (status = 202, body = OperationDocument)))]
async fn download_model(
    State(state): State<AgentState>,
    headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedToken>,
    Json(body): Json<DownloadRequest>,
) -> Response {
    let target = match resolve_download_target(&body, state.model_catalog.as_deref()) {
        Ok(value) => value,
        Err(error) => {
            return problem(StatusCode::BAD_REQUEST, error.code, error.detail);
        }
    };
    let Some(acquirer) = state.models.clone() else {
        return problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "spark.model.acquisition-unavailable",
            "model acquisition is unavailable",
        );
    };
    if body.dry_run {
        return match acquirer
            .resolve_selected(target.repository, target.revision, target.selection)
            .await
        {
            Ok(plan) => Json(DownloadPlanDocument {
                schema: "sy.spark.download-plan/v1".into(),
                repository: plan.repository.as_str().into(),
                commit: plan.commit.as_str().into(),
                artifacts: Some(plan.artifacts),
                logical_bytes: plan.logical_bytes,
                unique_bytes: plan.unique_bytes,
                temporary_bytes: plan.temporary_bytes,
                disk_reserve_bytes: acquirer.disk_reserve_bytes(),
            })
            .into_response(),
            Err(error) => acquisition_problem(error),
        };
    }
    let key = match required_idempotency(&headers) {
        Ok(value) => value,
        Err(()) => return missing_idempotency(),
    };
    let Some(database) = state.database.clone() else {
        return database_unavailable();
    };
    let request_hash = match super::wire::canonical_request_sha256(&body) {
        Ok(value) => value,
        Err(_) => {
            return problem(
                StatusCode::BAD_REQUEST,
                "spark.request.invalid",
                "download request cannot be canonicalized",
            );
        }
    };
    let accepted = match database
        .accept_operation(
            &auth.id,
            "model.download",
            &key,
            &request_hash,
            Some(body.repository.clone()),
        )
        .await
    {
        Ok(value) => value,
        Err(error) => return state_problem(error),
    };
    if !accepted.reused {
        let slots = Arc::clone(&state.download_slots);
        let operation_id = accepted.operation.id.clone();
        tokio::spawn(async move {
            run_download(DownloadJob {
                database,
                acquirer,
                slots,
                operation_id,
                repository: target.repository,
                revision: target.revision,
                alias: target.alias,
                selection: target.selection,
                update_alias: body.update_alias,
            })
            .await;
        });
    }
    accepted_response(accepted.operation)
}

struct DownloadJob {
    database: DbActor,
    acquirer: Arc<HubAcquirer>,
    slots: Arc<tokio::sync::Semaphore>,
    operation_id: String,
    repository: Repository,
    revision: Revision,
    alias: Option<Alias>,
    selection: ArtifactSelection,
    update_alias: bool,
}

async fn run_download(job: DownloadJob) {
    let DownloadJob {
        database,
        acquirer,
        slots,
        operation_id,
        repository,
        revision,
        alias,
        selection,
        update_alias,
    } = job;
    let Ok(_permit) = slots.acquire_owned().await else {
        return;
    };
    let plan = match acquirer
        .resolve_selected(repository, revision, selection)
        .await
    {
        Ok(plan) => plan,
        Err(error) => {
            fail_download(&database, &operation_id, error).await;
            return;
        }
    };
    let progress = OperationProgress {
        stage: "transferring".into(),
        current: Some(0),
        total: Some(plan.logical_bytes),
        unit: Some("bytes".into()),
        message: "resolved immutable commit; starting Rust Hub transfer".into(),
    };
    if database.transition(&operation_id, super::wire::OperationState::Running, progress, Some(serde_json::json!({"repository":plan.repository.as_str(),"commit":plan.commit.as_str(),"logical_bytes":plan.logical_bytes,"unique_bytes":plan.unique_bytes})), None).await.is_err() {
        return;
    }
    let (send_progress, mut receive_progress) = model::progress_channel();
    let progress_database = database.clone();
    let progress_operation = operation_id.clone();
    tokio::spawn(async move {
        while let Some(update) = receive_progress.recv().await {
            let _ = progress_database
                .transition(
                    &progress_operation,
                    super::wire::OperationState::Running,
                    OperationProgress {
                        stage: "transferring".into(),
                        current: Some(update.current_bytes),
                        total: update.total_bytes,
                        unit: Some("bytes".into()),
                        message: update
                            .file
                            .map(|file| format!("transferring {file}"))
                            .unwrap_or_else(|| "transferring immutable snapshot".into()),
                    },
                    None,
                    None,
                )
                .await;
        }
    });
    let acquire = acquirer.acquire(&plan, alias, send_progress);
    tokio::pin!(acquire);
    let result = loop {
        tokio::select! {
            result = &mut acquire => break Some(result),
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                if database.operation(&operation_id).await.ok().is_some_and(|operation| operation.state == super::wire::OperationState::Cancelled) {
                    debug_assert!(!model::should_run_fallback(TransferFailure::Cancelled, 0));
                    break None;
                }
            }
        }
    };
    let Some(result) = result else { return };
    match result {
        Ok(model) => match database.promote_model(model.clone(), update_alias).await {
            Ok(model) => {
                let _ = database
                    .transition(
                        &operation_id,
                        super::wire::OperationState::Succeeded,
                        OperationProgress {
                            stage: "complete".into(),
                            current: Some(model.logical_bytes),
                            total: Some(model.logical_bytes),
                            unit: Some("bytes".into()),
                            message: "immutable snapshot verified and promoted".into(),
                        },
                        serde_json::to_value(model).ok(),
                        None,
                    )
                    .await;
            }
            Err(error) => {
                let _ = fail_state_download(&database, &operation_id, error).await;
            }
        },
        Err(error) => fail_download(&database, &operation_id, error).await,
    }
}

async fn fail_download(database: &DbActor, id: &str, error: model::AcquisitionError) {
    let problem = acquisition_problem_document(error);
    let _ = database
        .transition(
            id,
            super::wire::OperationState::Failed,
            OperationProgress {
                stage: "failed".into(),
                current: None,
                total: None,
                unit: None,
                message: problem.detail.clone(),
            },
            None,
            Some(problem),
        )
        .await;
}

async fn fail_state_download(database: &DbActor, id: &str, error: StateError) {
    let problem = state_problem_document(error);
    let _ = database
        .transition(
            id,
            super::wire::OperationState::Failed,
            OperationProgress {
                stage: "failed".into(),
                current: None,
                total: None,
                unit: None,
                message: problem.detail.clone(),
            },
            None,
            Some(problem),
        )
        .await;
}

#[utoipa::path(delete, path = "/api/sy.spark/v1/models/{id}", params(("id" = String, Path)), request_body = RemoveModelRequest, responses((status = 200, body = RemovalPlanDocument), (status = 202, body = OperationDocument)))]
async fn remove_model(
    State(state): State<AgentState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedToken>,
    Json(body): Json<RemoveModelRequest>,
) -> Response {
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    let Some(acquirer) = &state.models else {
        return problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "spark.model.acquisition-unavailable",
            "model acquisition is unavailable",
        );
    };
    let model = match database.model(&id).await {
        Ok(model) => model,
        Err(error) => return state_problem(error),
    };
    let repository = match Repository::parse(&model.repository) {
        Ok(value) => value,
        Err(_) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "spark.model.state-invalid",
                "stored model identity is invalid",
            );
        }
    };
    let commit = match model::CommitSha::parse(&model.commit) {
        Ok(value) => value,
        Err(_) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "spark.model.state-invalid",
                "stored model identity is invalid",
            );
        }
    };
    let plan = match model::plan_removal(
        acquirer.cache_root(),
        &repository,
        &commit,
        !model.active_instances.is_empty(),
    ) {
        Ok(plan) => plan,
        Err(_) => {
            return problem(
                StatusCode::CONFLICT,
                "spark.model.active",
                "active model data cannot be removed",
            );
        }
    };
    let document = RemovalPlanDocument {
        schema: REMOVAL_PLAN_SCHEMA.into(),
        model_id: model.id.clone(),
        snapshot_bytes: model.logical_bytes,
        reclaimable_bytes: plan.reclaimable_bytes,
        shared_bytes: model.logical_bytes.saturating_sub(plan.reclaimable_bytes),
        active_instances: model.active_instances.clone(),
        aliases: model.aliases.clone(),
        requires_confirmation: plan.reclaimable_bytes > 0,
    };
    if body.dry_run {
        return Json(document).into_response();
    }
    if document.requires_confirmation && !body.confirmed {
        return problem(
            StatusCode::BAD_REQUEST,
            "spark.confirmation.required",
            "model removal requires explicit confirmation",
        );
    }
    let key = match required_idempotency(&headers) {
        Ok(value) => value,
        Err(()) => return missing_idempotency(),
    };
    let request_hash = match super::wire::canonical_request_sha256(&body) {
        Ok(value) => value,
        Err(_) => {
            return problem(
                StatusCode::BAD_REQUEST,
                "spark.request.invalid",
                "removal request cannot be canonicalized",
            );
        }
    };
    let accepted = match database
        .accept_operation(
            &auth.id,
            "model.remove",
            &key,
            &request_hash,
            Some(model.id.clone()),
        )
        .await
    {
        Ok(value) => value,
        Err(error) => return state_problem(error),
    };
    if !accepted.reused {
        let running = database
            .transition(
                &accepted.operation.id,
                super::wire::OperationState::Running,
                OperationProgress {
                    stage: "removing".into(),
                    current: Some(0),
                    total: Some(plan.reclaimable_bytes),
                    unit: Some("bytes".into()),
                    message: "removing only unreferenced native-cache data".into(),
                },
                None,
                None,
            )
            .await;
        if running.is_err()
            || model::execute_removal(&plan).is_err()
            || database.remove_model(&model.id).await.is_err()
        {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "spark.model.remove-failed",
                "model removal failed closed",
            );
        }
        let _ = database
            .transition(
                &accepted.operation.id,
                super::wire::OperationState::Succeeded,
                OperationProgress {
                    stage: "complete".into(),
                    current: Some(plan.reclaimable_bytes),
                    total: Some(plan.reclaimable_bytes),
                    unit: Some("bytes".into()),
                    message: "unreferenced model data removed".into(),
                },
                serde_json::to_value(document).ok(),
                None,
            )
            .await;
    }
    match database.operation(&accepted.operation.id).await {
        Ok(operation) => accepted_response(operation),
        Err(error) => state_problem(error),
    }
}

#[utoipa::path(get, path = "/api/sy.spark/v1/status", responses((status = 200, body = StatusDocument)))]
async fn status(State(state): State<AgentState>, RawQuery(query): RawQuery) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    let database = match &state.database {
        Some(database) => match database.health().await {
            Ok(health) => Some(DatabaseHealth {
                available: true,
                journal_mode: health.journal_mode,
                synchronous: health.synchronous,
                foreign_keys: health.foreign_keys,
                backup_valid: health.backup_valid,
                queue_capacity: health.queue_capacity,
            }),
            Err(error) => return state_problem(error),
        },
        None => None,
    };
    let executor_health = executor_snapshot(&state).await.ok();
    let executor_ready = executor_health
        .as_ref()
        .is_some_and(|snapshot| snapshot.health.guard_heartbeat && snapshot.health.event_heartbeat);
    let degraded_reasons = if executor_ready {
        Vec::new()
    } else {
        vec![DegradedReason {
            code: "spark.executor.unavailable".into(),
            detail: "the privileged executor is unavailable".into(),
        }]
    };
    Json(StatusDocument {
        schema: STATUS_SCHEMA.into(),
        agent: env!("CARGO_PKG_VERSION").into(),
        executor: executor_health
            .as_ref()
            .map(|snapshot| snapshot.health.version.clone())
            .unwrap_or_else(|| "unavailable".into()),
        read_only: database.is_none() || !executor_ready,
        degraded_reasons,
        database,
        executor_health,
    })
    .into_response()
}

#[utoipa::path(get, path = "/api/sy.spark/v1/metrics", responses((status = 200, body = String, content_type = "text/plain")))]
async fn metrics(State(state): State<AgentState>, RawQuery(query): RawQuery) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    let (operations, instances, models) = match &state.database {
        Some(database) => {
            let operations = database.list_operations().await.map(|items| items.len());
            let instances = database.list_instances().await.map(|items| items.len());
            let models = database.list_models().await.map(|items| items.len());
            match (operations, instances, models) {
                (Ok(operations), Ok(instances), Ok(models)) => (operations, instances, models),
                _ => return database_unavailable(),
            }
        }
        None => (0, 0, 0),
    };
    let executor_ready = executor_snapshot(&state)
        .await
        .is_ok_and(|snapshot| snapshot.health.guard_heartbeat && snapshot.health.event_heartbeat);
    let body = format!(
        "# TYPE sy_spark_agent_up gauge\nsy_spark_agent_up 1\n# TYPE sy_spark_executor_ready gauge\nsy_spark_executor_ready {}\n# TYPE sy_spark_operations_total gauge\nsy_spark_operations_total {operations}\n# TYPE sy_spark_instances_total gauge\nsy_spark_instances_total {instances}\n# TYPE sy_spark_models_total gauge\nsy_spark_models_total {models}\n",
        usize::from(executor_ready)
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

#[derive(Clone)]
struct AuthenticatedToken {
    id: String,
    scopes: Vec<TokenScope>,
    inference: Arc<tokio::sync::Semaphore>,
}

impl AuthenticatedToken {
    fn admin(inference: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            id: "bootstrap-admin".into(),
            scopes: vec![TokenScope::Admin],
            inference,
        }
    }

    fn permits(&self, scope: &TokenScope) -> bool {
        self.scopes.contains(&TokenScope::Admin) || self.scopes.contains(scope)
    }

    async fn acquire_inference(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ()> {
        Arc::clone(&self.inference)
            .acquire_owned()
            .await
            .map_err(|_| ())
    }
}

#[utoipa::path(get, path = "/api/sy.spark/v1/operations", responses((status = 200, body = OperationListDocument)))]
async fn list_operations(State(state): State<AgentState>, RawQuery(query): RawQuery) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    match database.list_operations().await {
        Ok(operations) => Json(OperationListDocument {
            schema: OPERATION_LIST_SCHEMA.into(),
            operations,
        })
        .into_response(),
        Err(error) => state_problem(error),
    }
}

#[utoipa::path(get, path = "/api/sy.spark/v1/operations/{id}", params(("id" = String, Path)), responses((status = 200, body = OperationDocument)))]
async fn get_operation(
    State(state): State<AgentState>,
    AxumPath(id): AxumPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    if !valid_resource_id(&id) {
        return problem(
            StatusCode::BAD_REQUEST,
            "spark.request.invalid-id",
            "operation ID is invalid",
        );
    }
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    match database.operation(&id).await {
        Ok(operation) => Json(operation).into_response(),
        Err(error) => state_problem(error),
    }
}

#[utoipa::path(get, path = "/api/sy.spark/v1/operations/{id}/events", params(("id" = String, Path)), responses((status = 200, description = "resumable operation event stream")))]
async fn operation_events(
    State(state): State<AgentState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    let after = match headers.get("last-event-id").map(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
    }) {
        None => 0,
        Some(Some(value)) => value,
        Some(None) => {
            return problem(
                StatusCode::BAD_REQUEST,
                "spark.events.invalid-cursor",
                "Last-Event-ID must be an unsigned integer",
            );
        }
    };
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    match database.events(&id, after).await {
        Ok(events) => {
            let stream = futures_util::stream::iter(events.into_iter().map(|event| {
                Event::default()
                    .id(event.id.to_string())
                    .event("operation")
                    .json_data(event)
            }));
            Sse::new(stream).into_response()
        }
        Err(error) => state_problem(error),
    }
}

#[utoipa::path(delete, path = "/api/sy.spark/v1/operations/{id}", params(("id" = String, Path)), responses((status = 202, body = OperationDocument)))]
async fn cancel_operation(
    State(state): State<AgentState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedToken>,
) -> Response {
    if let Err(response) = require_executor_for_mutation(&state).await {
        return response;
    }
    if required_idempotency(&headers).is_err() {
        return missing_idempotency();
    }
    let Some(database) = state.database.clone() else {
        return database_unavailable();
    };
    match database.cancel(&id, &auth.id).await {
        Ok(operation) => {
            if operation.kind == "instance.serve" {
                if let (Some(target), Some(executor)) =
                    (operation.target.as_deref(), state.executor.clone())
                {
                    if let Ok(instance) = database.instance(target).await {
                        if instance.observed != InstanceObservedState::Healthy {
                            state.routes.drain(&instance.name, instance.generation);
                            tokio::spawn(async move {
                                cancel_prehealth_serve(&database, &executor, &instance).await;
                            });
                        }
                    }
                }
            }
            accepted_response(operation)
        }
        Err(error) => state_problem(error),
    }
}

#[utoipa::path(post, path = "/api/sy.spark/v1/tokens", request_body = TokenCreateRequest, responses((status = 202, body = TokenCreatedDocument)))]
async fn create_token(
    State(state): State<AgentState>,
    headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedToken>,
    Json(body): Json<TokenCreateRequest>,
) -> Response {
    if let Err(response) = require_executor_for_mutation(&state).await {
        return response;
    }
    let key = match required_idempotency(&headers) {
        Ok(value) => value,
        Err(()) => return missing_idempotency(),
    };
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    match database.create_token(&auth.id, &key, body).await {
        Ok(created) => {
            match database.auth_snapshot().await {
                Ok(snapshot) => state.store_auth(snapshot),
                Err(error) => return state_problem(error),
            }
            let location = format!("{API_BASE}/operations/{}", created.operation.id);
            let mut response = (
                StatusCode::ACCEPTED,
                Json(TokenCreatedDocument {
                    operation: created.operation,
                    token: created.token,
                    bearer_token: created.bearer_token,
                }),
            )
                .into_response();
            accepted_headers(&mut response, &location);
            response
        }
        Err(error) => state_problem(error),
    }
}

#[utoipa::path(get, path = "/api/sy.spark/v1/tokens", responses((status = 200, body = TokenListDocument)))]
async fn list_tokens(State(state): State<AgentState>, RawQuery(query): RawQuery) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    match database.list_tokens().await {
        Ok(tokens) => Json(TokenListDocument {
            schema: TOKEN_LIST_SCHEMA.into(),
            tokens,
        })
        .into_response(),
        Err(error) => state_problem(error),
    }
}

#[utoipa::path(delete, path = "/api/sy.spark/v1/tokens/{id}", params(("id" = String, Path)), responses((status = 202, body = OperationDocument)))]
async fn revoke_token(
    State(state): State<AgentState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedToken>,
) -> Response {
    if let Err(response) = require_executor_for_mutation(&state).await {
        return response;
    }
    let key = match required_idempotency(&headers) {
        Ok(value) => value,
        Err(()) => return missing_idempotency(),
    };
    let Some(database) = &state.database else {
        return database_unavailable();
    };
    match database.revoke_token(&auth.id, &key, &id).await {
        Ok(operation) => {
            match database.auth_snapshot().await {
                Ok(snapshot) => state.store_auth(snapshot),
                Err(error) => return state_problem(error),
            }
            state.limiter.retain_recent();
            state.limiter.shrink_to_fit();
            accepted_response(operation)
        }
        Err(error) => state_problem(error),
    }
}

#[utoipa::path(get, path = "/api/sy.spark/v1/doctor", responses((status = 200, body = DoctorDocument)))]
async fn doctor(State(state): State<AgentState>, RawQuery(query): RawQuery) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    let check = match executor_snapshot(&state).await {
        Ok(snapshot) if snapshot.health.guard_heartbeat && snapshot.health.event_heartbeat => {
            DoctorCheck {
                code: "spark.executor.ready".into(),
                status: "ok".into(),
                detail: format!(
                    "executor {} reports host {} and Docker {} API {} over Unix IPC; admission reserve {} bytes, emergency floor {} bytes, disk reserve {} bytes",
                    snapshot.health.version,
                    snapshot.host.hostname,
                    snapshot.docker.version,
                    snapshot.docker.api_version,
                    snapshot.resource_policy.system_reserve_bytes,
                    snapshot.resource_policy.emergency_available_floor_bytes,
                    snapshot.resource_policy.disk_reserve_bytes,
                ),
            }
        }
        _ => DoctorCheck {
            code: "spark.executor.unavailable".into(),
            status: "degraded".into(),
            detail: "read operations remain available; host mutations require the executor".into(),
        },
    };
    let mut checks = vec![check, peer_lateral_risk_check()];
    if let Some(database) = &state.database {
        if let Ok(instances) = database.list_instances().await {
            checks.extend(instances.into_iter().filter_map(|instance| {
                instance
                    .quarantine
                    .as_ref()
                    .map(|cause| DoctorCheck {
                        code: "spark.instance.quarantined".into(),
                        status: "degraded".into(),
                        detail: format!(
                            "instance {} generation {} has quarantined Docker evidence: {}",
                            instance.name, instance.generation, cause
                        ),
                    })
                    .or_else(|| {
                        instance.restart_suppressed.then(|| DoctorCheck {
                            code: "spark.instance.restart-suppressed".into(),
                            status: "degraded".into(),
                            detail: format!(
                                "instance {} generation {} exceeded its bounded restart budget",
                                instance.name, instance.generation
                            ),
                        })
                    })
            }));
        }
        if let Ok(evidence) = database.list_quarantine().await {
            checks.extend(evidence.into_iter().map(|evidence| DoctorCheck {
                code: "spark.docker.quarantined-evidence".into(),
                status: "degraded".into(),
                detail: format!(
                    "managed container {} is restart-disabled and quarantined: {}",
                    evidence
                        .container_id
                        .get(..12)
                        .unwrap_or(evidence.container_id.as_str()),
                    evidence.cause
                ),
            }));
        }
    }
    Json(DoctorDocument {
        schema: DOCTOR_SCHEMA.into(),
        checks,
    })
    .into_response()
}

fn peer_lateral_risk_check() -> DoctorCheck {
    DoctorCheck {
        code: "spark.network.peer-lateral-risk".into(),
        status: "accepted-risk".into(),
        detail: "the internal bridge blocks external egress, but managed engine containers can reach other managed engine peers on that bridge".into(),
    }
}

async fn executor_snapshot(state: &AgentState) -> Result<ExecutorSnapshot, ()> {
    let executor = state.executor.as_ref().ok_or(())?;
    executor.snapshot().await.map_err(|_| ())
}

async fn require_executor_for_mutation(state: &AgentState) -> Result<(), Response> {
    #[cfg(test)]
    if state.executor_ready_override {
        return Ok(());
    }
    match executor_snapshot(state).await {
        Ok(snapshot) if snapshot.health.guard_heartbeat && snapshot.health.event_heartbeat => {
            Ok(())
        }
        _ => Err(problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "spark.executor.unavailable",
            "the privileged executor is unavailable; authenticated reads remain available",
        )),
    }
}

#[utoipa::path(get, path = "/api/sy.spark/v1/certificates/status", responses((status = 200, body = CertificateStatusDocument)))]
async fn certificate_status(
    State(state): State<AgentState>,
    RawQuery(query): RawQuery,
) -> Response {
    if let Some(response) = reject_query(query) {
        return response;
    }
    Json(state.certificate).into_response()
}

fn certificate_status_from_pem(pem: &[u8]) -> anyhow::Result<CertificateStatusDocument> {
    use x509_parser::{extensions::GeneralName, parse_x509_certificate, pem::parse_x509_pem};

    let (_, pem) =
        parse_x509_pem(pem).map_err(|error| anyhow::anyhow!("parse leaf PEM: {error}"))?;
    let (_, certificate) = parse_x509_certificate(&pem.contents)
        .map_err(|error| anyhow::anyhow!("parse leaf certificate: {error}"))?;
    let mut dns_sans = Vec::new();
    let mut ip_sans = Vec::new();
    if let Some(san) = certificate.subject_alternative_name()? {
        for name in &san.value.general_names {
            match name {
                GeneralName::DNSName(name) => dns_sans.push((*name).to_owned()),
                GeneralName::IPAddress([a, b, c, d]) => {
                    ip_sans.push(std::net::Ipv4Addr::new(*a, *b, *c, *d).to_string());
                }
                GeneralName::IPAddress(bytes) if bytes.len() == 16 => {
                    let octets: [u8; 16] = (*bytes).try_into()?;
                    ip_sans.push(std::net::Ipv6Addr::from(octets).to_string());
                }
                _ => {}
            }
        }
    }
    dns_sans.sort();
    dns_sans.dedup();
    ip_sans.sort();
    ip_sans.dedup();
    Ok(CertificateStatusDocument {
        schema: CERTIFICATE_SCHEMA.into(),
        valid: certificate.validity().is_valid(),
        dns_sans,
        ip_sans,
    })
}

async fn authenticate(
    State(state): State<AgentState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let request_path = request.uri().path().to_owned();
    if headers.contains_key(header::ORIGIN) {
        return authentication_layer_error(
            &request_path,
            StatusCode::FORBIDDEN,
            "spark.http.cors-rejected",
            "cross-origin requests are not permitted",
        );
    }
    if let Some(peer) = request.extensions().get::<ConnectInfo<SocketAddr>>() {
        if !state
            .allowed_clients
            .iter()
            .any(|cidr| cidr.contains(peer.ip()))
        {
            return authentication_layer_error(
                &request_path,
                StatusCode::FORBIDDEN,
                "spark.network.source-rejected",
                "client source is outside the configured CIDR policy",
            );
        }
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let api_key = request
        .uri()
        .path()
        .starts_with("/anthropic/")
        .then(|| headers.get("x-api-key"))
        .flatten()
        .and_then(|value| value.to_str().ok());
    let presented = match (bearer, api_key) {
        (Some(token), None) | (None, Some(token)) => Some(token),
        _ => None,
    };
    let Some(presented) = presented else {
        return authentication_layer_error(
            &request_path,
            StatusCode::UNAUTHORIZED,
            "spark.auth.failed",
            "authentication failed",
        );
    };
    let auth = if token_matches(state.token.expose_secret(), presented) {
        AuthenticatedToken::admin(state.inference_slot("bootstrap-admin", 64))
    } else {
        let Some((id, secret)) = parse_bearer(presented) else {
            return auth_failed(&request_path);
        };
        let snapshot = state.auth.load();
        let Some(verifier) = snapshot.tokens.get(id) else {
            return auth_failed(&request_path);
        };
        if !verifier.verify(
            &state
                .database
                .as_ref()
                .map(DbActor::pepper)
                .unwrap_or_else(|| Arc::new(SecretString::from("invalid"))),
            secret,
        ) {
            return auth_failed(&request_path);
        }
        if let Some(peer) = request.extensions().get::<ConnectInfo<SocketAddr>>() {
            if !verifier.token.allowed_cidrs.is_empty()
                && !verifier
                    .token
                    .allowed_cidrs
                    .iter()
                    .filter_map(|value| value.parse::<Cidr>().ok())
                    .any(|cidr| cidr.contains(peer.ip()))
            {
                return auth_failed(&request_path);
            }
        }
        AuthenticatedToken {
            id: id.into(),
            scopes: verifier.token.scopes.clone(),
            inference: state.inference_slot(id, verifier.token.max_concurrent_inference),
        }
    };
    let required = required_scope(request.method(), request.uri().path());
    if required.as_ref().is_some_and(|scope| !auth.permits(scope)) {
        return auth_failed(&request_path);
    }
    let limiter_key = format!(
        "{}:{}",
        auth.id,
        required.as_ref().map(TokenScope::as_str).unwrap_or("none")
    );
    if state.limiter.check_key(&limiter_key).is_err() {
        let mut response = authentication_layer_error(
            &request_path,
            StatusCode::TOO_MANY_REQUESTS,
            "spark.rate.exceeded",
            "request rate exceeded",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    }
    request.headers_mut().remove(header::AUTHORIZATION);
    request.headers_mut().remove("x-api-key");
    request.extensions_mut().insert(auth);
    next.run(request).await
}

fn parse_bearer(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("sy_")?;
    let (id, secret) = rest.split_once('_')?;
    if id.len() == 26 && secret.len() == 64 && secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some((id, secret))
    } else {
        None
    }
}

fn required_scope(method: &Method, path: &str) -> Option<TokenScope> {
    if path.starts_with("/openai/") || path.starts_with("/anthropic/") {
        return Some(TokenScope::Inference);
    }
    if path == format!("{API_BASE}/status") {
        return Some(TokenScope::InstancesRead);
    }
    if path == format!("{API_BASE}/metrics") {
        return Some(TokenScope::Admin);
    }
    if path.starts_with(&format!("{API_BASE}/operations")) {
        return Some(if method == Method::DELETE {
            TokenScope::OperationsCancel
        } else {
            TokenScope::OperationsRead
        });
    }
    if path.starts_with(&format!("{API_BASE}/instances/")) && path.ends_with("/logs") {
        return Some(TokenScope::LogsRead);
    }
    if path == format!("{API_BASE}/instances")
        || path.starts_with(&format!("{API_BASE}/instances/"))
    {
        return Some(if method == Method::GET {
            TokenScope::InstancesRead
        } else {
            TokenScope::InstancesWrite
        });
    }
    if path.starts_with(&format!("{API_BASE}/tokens"))
        || path == format!("{API_BASE}/doctor")
        || path.starts_with(&format!("{API_BASE}/certificates"))
    {
        return Some(TokenScope::Admin);
    }
    if path.starts_with(&format!("{API_BASE}/models")) {
        return Some(if method == Method::DELETE {
            TokenScope::ModelsWrite
        } else {
            TokenScope::ModelsRead
        });
    }
    if path == format!("{API_BASE}/downloads") {
        return Some(TokenScope::ModelsWrite);
    }
    if path == format!("{API_BASE}/admission") {
        return Some(TokenScope::InstancesWrite);
    }
    None
}

fn auth_failed(path: &str) -> Response {
    authentication_layer_error(
        path,
        StatusCode::UNAUTHORIZED,
        "spark.auth.failed",
        "authentication failed",
    )
}

fn authentication_layer_error(
    path: &str,
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> Response {
    if path.starts_with("/anthropic/") {
        let error_type = match status {
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::FORBIDDEN => "permission_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            _ => "invalid_request_error",
        };
        anthropic_error(
            status,
            gateway::AnthropicError {
                error_type,
                message,
            },
        )
    } else {
        problem(status, code, message)
    }
}

fn token_matches(expected: &str, presented: &str) -> bool {
    let Ok(mut expected_mac) = <HmacSha256 as Mac>::new_from_slice(expected.as_bytes()) else {
        return false;
    };
    expected_mac.update(expected.as_bytes());
    let digest = expected_mac.finalize().into_bytes();
    let Ok(mut presented_mac) = <HmacSha256 as Mac>::new_from_slice(expected.as_bytes()) else {
        return false;
    };
    presented_mac.update(presented.as_bytes());
    presented_mac.verify_slice(&digest).is_ok()
}

fn reject_query(query: Option<String>) -> Option<Response> {
    if query.as_deref().is_none_or(str::is_empty) {
        None
    } else {
        Some(problem(
            StatusCode::BAD_REQUEST,
            "spark.request.unknown-field",
            "query fields are not accepted on this route",
        ))
    }
}

async fn not_found() -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "spark.route.not-found",
        "route is not in the Spark API allowlist",
    )
}

fn problem(status: StatusCode, code: &str, detail: &str) -> Response {
    let body = Json(ProblemDocument {
        schema: PROBLEM_SCHEMA.into(),
        r#type: format!("https://sy.local/problems/{code}"),
        code: code.into(),
        status: status.as_u16(),
        detail: detail.into(),
        remediation: Vec::new(),
        operation_id: None,
    });
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/problem+json"),
    );
    response
}

fn state_problem(error: StateError) -> Response {
    match error {
        StateError::Overloaded => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "spark.state.overloaded",
            "durable state queue is saturated",
        ),
        StateError::Conflict(detail) => {
            problem(StatusCode::CONFLICT, "spark.idempotency.conflict", &detail)
        }
        StateError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "spark.resource.not-found",
            "resource was not found",
        ),
        StateError::Invalid(detail) => {
            problem(StatusCode::BAD_REQUEST, "spark.request.invalid", &detail)
        }
        StateError::Unavailable(_) => database_unavailable(),
    }
}

fn state_problem_document(error: StateError) -> ProblemDocument {
    let (status, code, detail) = match error {
        StateError::Overloaded => (
            503,
            "spark.state.overloaded",
            "durable state queue is saturated".into(),
        ),
        StateError::Conflict(detail) => (409, "spark.state.conflict", detail),
        StateError::NotFound => (
            404,
            "spark.resource.not-found",
            "resource was not found".into(),
        ),
        StateError::Invalid(detail) => (400, "spark.request.invalid", detail),
        StateError::Unavailable(_) => (
            503,
            "spark.database.unavailable",
            "durable Spark state is unavailable".into(),
        ),
    };
    ProblemDocument {
        schema: PROBLEM_SCHEMA.into(),
        r#type: format!("https://sy.local/problems/{code}"),
        code: code.into(),
        status,
        detail,
        remediation: Vec::new(),
        operation_id: None,
    }
}

fn acquisition_problem(error: model::AcquisitionError) -> Response {
    let document = acquisition_problem_document(error);
    let status = StatusCode::from_u16(document.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, Json(document)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
}

fn acquisition_problem_document(error: model::AcquisitionError) -> ProblemDocument {
    let (status, code) = match error.failure {
        TransferFailure::Authentication => (403, "spark.hub.authentication"),
        TransferFailure::NotFound => (404, "spark.hub.not-found"),
        TransferFailure::DiskReserve => (409, "spark.disk.reserve"),
        TransferFailure::Policy => (409, "spark.model.policy"),
        TransferFailure::Cancelled => (409, "spark.operation.cancelled"),
        TransferFailure::XetTransport
        | TransferFailure::XetIntegrity
        | TransferFailure::NoProgress
        | TransferFailure::Other => (500, "spark.model.download-failed"),
    };
    ProblemDocument {
        schema: PROBLEM_SCHEMA.into(),
        r#type: format!("https://sy.local/problems/{code}"),
        code: code.into(),
        status,
        detail: error.detail.into(),
        remediation: Vec::new(),
        operation_id: None,
    }
}

fn database_unavailable() -> Response {
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "spark.database.unavailable",
        "durable Spark state is unavailable",
    )
}

fn required_idempotency(headers: &HeaderMap) -> Result<String, ()> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(str::to_owned)
        .ok_or(())
}

fn missing_idempotency() -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "spark.idempotency.required",
        "Idempotency-Key is required",
    )
}

fn valid_resource_id(id: &str) -> bool {
    id.len() == 26 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn valid_model_id(id: &str) -> bool {
    id.len() == 34
        && id.starts_with("m_")
        && id[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn accepted_response(operation: OperationDocument) -> Response {
    let location = format!("{API_BASE}/operations/{}", operation.id);
    let mut response = (StatusCode::ACCEPTED, Json(operation)).into_response();
    accepted_headers(&mut response, &location);
    response
}

fn accepted_headers(response: &mut Response, location: &str) {
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
}

pub async fn serve(
    config_path: &Path,
    certificate_path: &Path,
    key_path: &Path,
    token_path: &Path,
    hf_token_path: &Path,
) -> anyhow::Result<()> {
    let config: AgentConfig = toml::from_str(&tokio::fs::read_to_string(config_path).await?)?;
    anyhow::ensure!(
        config.schema == "sy.spark.agent/v1",
        "unsupported Spark agent configuration schema"
    );
    anyhow::ensure!(
        config.engine_catalog == Path::new("/etc/sy/spark/engines"),
        "engine catalog must use the fixed root-owned path"
    );
    anyhow::ensure!(
        config.model_catalog == Path::new("/etc/sy/spark/models.toml"),
        "model catalog must use the fixed root-owned path"
    );
    let engine_catalog =
        super::engine::EngineCatalog::load(&config.engine_catalog).map_err(anyhow::Error::msg)?;
    let model_catalog = super::model_catalog::ModelCatalog::load(&config.model_catalog)
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        config.models.endpoint.starts_with("https://")
            && config.models.cache_root.is_absolute()
            && config.models.fallback_executable.is_absolute()
            && config.models.no_progress_seconds > 0,
        "Spark model acquisition policy is invalid"
    );
    for name in [
        "HF_TOKEN",
        "HF_TOKEN_PATH",
        "HF_HOME",
        "HF_HUB_CACHE",
        "HF_ENDPOINT",
    ] {
        anyhow::ensure!(
            std::env::var_os(name).is_none(),
            "ambient Hugging Face configuration is forbidden"
        );
    }
    anyhow::ensure!(
        !config.listen.ip().is_unspecified(),
        "Spark agent listen address must be explicit"
    );
    anyhow::ensure!(
        config.plain_http_loopback_only,
        "plain HTTP must remain loopback-only"
    );
    config.resources.policy().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        config.operations.max_parallel_downloads > 0
            && config.operations.max_parallel_starts > 0
            && config.operations.max_parallel_tunes > 0,
        "Spark operation concurrency must be bounded above zero"
    );
    anyhow::ensure!(
        config.retention.operation_days > 0 && config.retention.database_backups > 0,
        "Spark retention policy must be positive"
    );
    let allowed_clients = config
        .allowed_client_cidrs
        .iter()
        .map(|value| value.parse::<Cidr>())
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        !allowed_clients.is_empty(),
        "Spark allowed-client CIDRs must not be empty"
    );
    let token = tokio::fs::read_to_string(token_path).await?;
    let hf_token = match tokio::fs::read_to_string(hf_token_path).await {
        Ok(value) if !value.trim().is_empty() => Some(SecretString::from(value.trim().to_owned())),
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let certificate = tokio::fs::read(certificate_path).await?;
    let key = tokio::fs::read(key_path).await?;
    let certificate_status = certificate_status_from_pem(&certificate)?;
    let tls = tls13_config(certificate, key).await?;
    let database = DbActor::open(
        "/var/lib/sy-spark/state.sqlite3",
        "/var/lib/sy-spark/backups",
        DATABASE_QUEUE_CAPACITY,
        config.retention.database_backups as usize,
        SecretString::from(token.trim().to_owned()),
    )?;
    let executor = ExecutorClient::new(&config.executor_socket);
    let executor = match executor.emergency_records().await {
        Ok(records) => {
            for record in records {
                database.import_emergency(record).await?;
            }
            Some(executor)
        }
        Err(_) => None,
    };
    let mut state = AgentState::new(token.trim(), Vec::new(), Vec::new())
        .with_engine_catalog(engine_catalog)
        .with_model_catalog(model_catalog)
        .with_database(database.clone())
        .await?
        .with_models(
            HubAcquirer::new(
                config.models.cache_root,
                config.models.endpoint,
                hf_token,
                config
                    .resources
                    .disk_reserve_gib
                    .checked_mul(1024 * 1024 * 1024)
                    .ok_or_else(|| anyhow::anyhow!("Spark disk reserve overflow"))?,
                Duration::from_secs(config.models.no_progress_seconds),
                Some(FallbackConfig {
                    executable: config.models.fallback_executable,
                    credential: hf_token_path.to_owned(),
                }),
            )?,
            config.operations.max_parallel_downloads,
        )
        .with_start_slots(config.operations.max_parallel_starts);
    if let Some(executor) = executor {
        state = state.with_executor(executor);
    }
    state.certificate = certificate_status;
    let state = state.with_allowed_clients(allowed_clients);
    if let Some(executor) = state.executor.clone() {
        let routes = state.routes.clone();
        let coordinator = state.admission.clone();
        let event_epoch = executor
            .health()
            .await
            .map(|health| health.event_epoch)
            .unwrap_or(0);
        let _ = reconcile_once(&database, &executor, Some(&routes), &coordinator).await;
        let reconcile_database = database.clone();
        tokio::spawn(reconciliation_loop(
            reconcile_database,
            executor,
            routes,
            coordinator,
            event_epoch,
        ));
    }
    let _contract = ApiDoc::openapi();
    let handle = axum_server::Handle::new();
    let shutdown = handle.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        sy_core::notify::stopping();
        shutdown.graceful_shutdown(Some(Duration::from_secs(10)));
    });
    sy_core::notify::ready();
    let _watchdog = sy_core::notify::spawn_watchdog();
    spawn_tls_reload(
        tls.clone(),
        certificate_path.to_owned(),
        key_path.to_owned(),
    );
    axum_server::bind_rustls(config.listen, tls)
        .handle(handle)
        .serve(router(state).into_make_service_with_connect_info::<SocketAddr>())
        .await?;
    database.shutdown()?;
    Ok(())
}

fn spawn_tls_reload(
    tls: axum_server::tls_rustls::RustlsConfig,
    certificate: PathBuf,
    key: PathBuf,
) {
    tokio::spawn(async move {
        let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            tracing::error!(
                component = "spark-agent",
                event_code = "spark.tls.signal-failed"
            );
            return;
        };
        while signal.recv().await.is_some() {
            match reload_tls_from_pem_files(&tls, &certificate, &key).await {
                Ok(()) => {
                    tracing::info!(component = "spark-agent", event_code = "spark.tls.reloaded")
                }
                Err(_) => tracing::error!(
                    component = "spark-agent",
                    event_code = "spark.tls.reload-failed"
                ),
            }
        }
    });
}

async fn reload_tls_from_pem_files(
    tls: &axum_server::tls_rustls::RustlsConfig,
    certificate: &Path,
    key: &Path,
) -> anyhow::Result<()> {
    let replacement = tls13_config(
        tokio::fs::read(certificate).await?,
        tokio::fs::read(key).await?,
    )
    .await?;
    tls.reload_from_config(replacement.get_inner());
    Ok(())
}

async fn reconciliation_loop(
    database: DbActor,
    executor: ExecutorClient,
    routes: RouteRegistry,
    coordinator: TransitionCoordinator,
    mut event_epoch: u64,
) {
    let mut closure = tokio::time::interval(RECONCILE_INTERVAL);
    closure.tick().await;
    let mut events = tokio::time::interval(EVENT_EPOCH_POLL_INTERVAL);
    events.tick().await;
    loop {
        tokio::select! {
            _ = closure.tick() => {
                let _ = reconcile_once(&database, &executor, Some(&routes), &coordinator).await;
            }
            _ = events.tick() => {
                let Ok(health) = executor.health().await else {
                    continue;
                };
                if let Some(next_epoch) = changed_event_epoch(event_epoch, &health) {
                    event_epoch = next_epoch;
                    let _ = reconcile_once(&database, &executor, Some(&routes), &coordinator).await;
                }
            }
        }
    }
}

fn changed_event_epoch(previous: u64, health: &super::wire::ExecutorHealth) -> Option<u64> {
    (health.event_heartbeat && health.event_epoch != previous).then_some(health.event_epoch)
}

async fn tls13_config(
    certificate: Vec<u8>,
    key: Vec<u8>,
) -> anyhow::Result<axum_server::tls_rustls::RustlsConfig> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let certificates =
        CertificateDer::pem_slice_iter(&certificate).collect::<Result<Vec<_>, _>>()?;
    let private_key = PrivateKeyDer::from_pem_slice(&key)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)?;
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        Arc::new(server),
    ))
}

#[cfg(test)]
mod tests {
    use super::{resolve_download_target, router, AgentState, ApiDoc, API_BASE};
    use axum::{
        body::Body,
        http::{header, Request},
    };
    use std::{
        net::SocketAddr,
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tower::ServiceExt;
    use utoipa::OpenApi;

    #[test]
    fn configured_alias_download_persists_exact_artifact_identity() {
        let catalog = crate::spark::model_catalog::ModelCatalog::parse(include_str!(
            "../../configs/sy/spark/models.toml"
        ))
        .unwrap();
        let request = crate::spark::wire::DownloadRequest {
            repository: "qwen3.8:27b".into(),
            revision: "main".into(),
            alias: None,
            artifact: None,
            auxiliary: Vec::new(),
            update_alias: false,
            dry_run: false,
        };

        let target = resolve_download_target(&request, Some(&catalog)).unwrap();

        assert_eq!(target.alias.unwrap().as_str(), "qwen3.8:27b");
        assert_eq!(
            target.selection.configured_artifacts().unwrap(),
            catalog.resolve("qwen3.8:27b").unwrap().artifacts()
        );
    }

    #[test]
    fn arbitrary_repository_download_uses_automatic_artifact_selection() {
        let request = crate::spark::wire::DownloadRequest {
            repository: "owner/model".into(),
            revision: "main".into(),
            alias: None,
            artifact: None,
            auxiliary: Vec::new(),
            update_alias: false,
            dry_run: false,
        };
        assert!(resolve_download_target(&request, None).is_ok());
    }

    const TOKEN: &str = "test-bootstrap-token-with-at-least-256-bits-of-random-material";

    fn llama_chat_sse() -> String {
        [
            r#"{"choices":[{"delta":{"reasoning_content":"inspect"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"content":"ready"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}},{"index":1,"id":"call_2","function":{"name":"patch","arguments":"{\"path\":\"a\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
            + "data: [DONE]\n\n"
    }

    async fn llama_chat_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16_384];
            let count = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("POST /v1/chat/completions "));
            assert!(request.contains(r#""stream_options":{"include_usage":true}"#));
            let body = llama_chat_sse();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        (address, task)
    }

    #[test]
    fn legacy_recipe_benchmark_and_tuning_paths_are_not_public() {
        let document = serde_json::to_value(ApiDoc::openapi()).unwrap();
        let paths = document["paths"].as_object().unwrap();
        for path in [
            "/api/sy.spark/v1/recipes",
            "/api/sy.spark/v1/benchmarks",
            "/api/sy.spark/v1/tunings",
        ] {
            assert!(!paths.contains_key(path));
        }
    }

    #[tokio::test]
    async fn metrics_are_authenticated_bounded_and_content_free() {
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let denied = router(state.clone())
            .oneshot(
                Request::get("/api/sy.spark/v1/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), axum::http::StatusCode::UNAUTHORIZED);

        let allowed = router(state)
            .oneshot(
                Request::get("/api/sy.spark/v1/metrics")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(allowed.into_body(), 4096)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("sy_spark_agent_up 1"));
        for forbidden in ["prompt", "token", "authorization", "commit", "client_id"] {
            assert!(!text.to_ascii_lowercase().contains(forbidden));
        }
    }

    #[tokio::test]
    async fn inference_route_waits_for_semantic_publication_and_rewrites_identity() {
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        state.routes.mark_warming("ornith", 1);
        let request = || {
            Request::get("/openai/ornith/v1/models")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap()
        };
        let warming = router(state.clone()).oneshot(request()).await.unwrap();
        assert_eq!(
            warming.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(warming.headers()[header::RETRY_AFTER], "1");

        let upstream = crate::spark::upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            "172.30.0.2".parse().unwrap(),
            8000,
            [("GET", "/v1/models"), ("POST", "/v1/completions")],
        )
        .unwrap();
        state.routes.publish(
            "ornith",
            "ornith-1.5:9b".into(),
            "Ornith-1.5-9B".into(),
            upstream,
        );
        let ready = router(state).oneshot(request()).await.unwrap();
        let body = axum::body::to_bytes(ready.into_body(), 65_536)
            .await
            .unwrap();
        let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(document["data"][0]["id"], "ornith-1.5:9b");
        assert!(!String::from_utf8_lossy(&body).contains("172.30.0.2"));
    }

    #[tokio::test]
    async fn openai_stream_from_llama_fixture_is_protocol_complete() {
        let (address, server) = llama_chat_server().await;
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let upstream = crate::spark::upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [("POST", "/v1/chat/completions")],
        )
        .unwrap();
        state.routes.publish(
            "fixture",
            "public-model".into(),
            "served-model".into(),
            upstream,
        );
        let request = Request::post("/openai/fixture/v1/chat/completions")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(r#"{"model":"public-model","messages":[{"role":"user","content":"work"}],"stream":true}"#))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 65_536)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let fields = [
            "reasoning_content",
            "ready",
            "call_1",
            "call_2",
            "tool_calls",
            "prompt_tokens",
            "[DONE]",
        ];
        assert!(
            fields
                .windows(2)
                .all(|pair| body.find(pair[0]).unwrap() < body.rfind(pair[1]).unwrap()),
            "{body}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_responses_route_preserves_tool_history_and_engine_sse() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16_384];
            let count = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("POST /v1/responses "));
            assert!(request.contains(r#""model":"public-model""#));
            assert!(request.contains(r#""type":"function_call_output""#));
            let body = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let upstream = crate::spark::upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [("POST", "/v1/responses")],
        )
        .unwrap();
        let mut profile = crate::spark::gateway::GatewayProfile::text();
        profile.native_responses = true;
        state.routes.publish_with_profile(
            "fixture",
            "public-model".into(),
            "served-model".into(),
            profile,
            upstream,
        );
        let request = Request::post("/openai/fixture/v1/responses")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(r#"{"model":"client-model","input":[{"type":"message","role":"user","content":"use the tool"},{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"VALUE-42"}],"stream":true}"#))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 65_536)
            .await
            .unwrap();
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert!(String::from_utf8_lossy(&body).contains("response.completed"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn anthropic_stream_from_llama_fixture_is_protocol_complete() {
        let (address, server) = llama_chat_server().await;
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let upstream = crate::spark::upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [("POST", "/v1/chat/completions")],
        )
        .unwrap();
        state.routes.publish(
            "fixture",
            "public-model".into(),
            "served-model".into(),
            upstream,
        );
        let request = Request::post("/anthropic/fixture/v1/messages")
            .header("x-api-key", TOKEN)
            .header("anthropic-version", "2023-06-01")
            .body(Body::from(r#"{"model":"public-model","messages":[{"role":"user","content":"work"}],"max_tokens":64,"stream":true}"#))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 65_536)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let fields = [
            "message_start",
            "thinking_delta",
            "text_delta",
            "call_1",
            "call_2",
            "input_tokens",
            "message_stop",
        ];
        assert!(
            fields
                .windows(2)
                .all(|pair| body.find(pair[0]).unwrap() < body.rfind(pair[1]).unwrap()),
            "{body}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn client_cancellation_closes_the_llama_upstream() {
        use http_body_util::BodyExt as _;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (closed, close_seen) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"ready\"},\"finish_reason\":null}]}\n\n").await.unwrap();
            let mut byte = [0; 1];
            assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
            let _ = closed.send(());
        });
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let upstream = crate::spark::upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [("POST", "/v1/chat/completions")],
        )
        .unwrap();
        state.routes.publish(
            "fixture",
            "public-model".into(),
            "served-model".into(),
            upstream,
        );
        let request = Request::post("/openai/fixture/v1/chat/completions")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(
                r#"{"messages":[{"role":"user","content":"work"}],"stream":true}"#,
            ))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        let mut body = response.into_body();
        assert!(body.frame().await.is_some());
        drop(body);
        tokio::time::timeout(Duration::from_secs(1), close_seen)
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn incomplete_protocol_probe_cannot_publish_route() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for index in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                let body = match index {
                    0 => r#"{"data":[{"id":"served-model"}]}"#,
                    1 => r#"{"id":"cmpl-probe","object":"text_completion","model":"served-model","choices":[{"index":0,"text":"OK","finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#,
                    _ => "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                };
                let content_type = if index == 2 {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let route = crate::spark::upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [
                ("GET", "/v1/models"),
                ("POST", "/v1/completions"),
                ("POST", "/v1/chat/completions"),
            ],
        )
        .unwrap();
        let observed: crate::spark::executor::ObservedEngine = serde_json::from_value(serde_json::json!({
            "instance_id":"i_11111111111111111111111111111111","generation":1,
            "container_id":"container","network_id":"network","address":address.ip().to_string(),"port":address.port(),
            "running":true,"restart_policy":"no","health_method":"GET","health_path":"/health",
            "allowed_routes":[],"gateway_profile":{"capabilities":["text_generation"],"vision":null,"embeddings":null,"startup_protocol_probe":true,"sampling":{"defaults":{}}},
            "served_model":"served-model","semantic_prompt":"probe","semantic_max_tokens":1,
            "startup_deadline_seconds":5,"init_pid":1,"pid_start_time_ticks":1,"cgroup_path":"/fixture"
        })).unwrap();
        let routes = crate::spark::gateway::RouteRegistry::default();
        routes.mark_warming("fixture", 1);
        assert!(
            super::semantic_probe_result(&route, &observed, Duration::from_secs(1))
                .await
                .is_err()
        );
        assert!(matches!(
            routes.lookup("fixture"),
            crate::spark::gateway::RouteLookup::Warming
        ));
        server.await.unwrap();
    }

    #[test]
    fn configured_health_body_rejects_loading_and_accepts_ok() {
        let rule = crate::spark::engine::EngineHealthBody {
            json_pointer: "/status".into(),
            equals: "ok".into(),
        };
        let response = |status| crate::spark::upstream::UpstreamResponse {
            status: 200,
            bytes: format!(r#"{{"status":"{status}"}}"#).into_bytes(),
        };

        assert!(!super::health_response_is_ready(
            &response("loading"),
            Some(&rule)
        ));
        assert!(super::health_response_is_ready(
            &response("ok"),
            Some(&rule)
        ));
    }

    #[tokio::test]
    async fn stopped_intent_interrupts_engine_readiness_wait() {
        let (_state, database, _root) = durable_state().await;
        let model = ornith_model();
        database.promote_model(model.clone(), false).await.unwrap();
        let operation = database
            .accept_operation(
                "bootstrap",
                "instance.serve",
                "readiness-stop",
                &"a".repeat(64),
                Some("ornith".into()),
            )
            .await
            .unwrap()
            .operation;
        let instance = database
            .begin_serve(creating_instance(&model))
            .await
            .unwrap()
            .instance;
        database.begin_stop(&instance.id).await.unwrap();
        let route = crate::spark::upstream::ObservedRoute::new(
            &instance.id,
            instance.generation,
            "127.0.0.1".parse().unwrap(),
            9,
            [("GET", "/health")],
        )
        .unwrap();
        let request = route.request("GET", "/health", 0).unwrap();
        let socket = _root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(sy_ipc::Server::new(ResourceExecutor).serve(listener));
        let interrupt = super::ReadinessInterrupt::new(
            database.clone(),
            operation.id,
            instance.id,
            crate::spark::executor::ExecutorClient::new(socket),
            instance.generation,
        );

        let ready = tokio::time::timeout(
            Duration::from_millis(100),
            super::wait_until_engine_ready(
                &route,
                &request,
                None,
                Duration::from_secs(5),
                Some(&interrupt),
            ),
        )
        .await
        .expect("stopped intent must interrupt readiness");

        assert!(!ready);
        database.shutdown().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn exited_container_interrupts_engine_readiness_wait() {
        let (_state, database, root) = durable_state().await;
        let socket = root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(sy_ipc::Server::new(StoppedResourceExecutor).serve(listener));
        let instance_id = "i_11111111111111111111111111111111";
        let route = crate::spark::upstream::ObservedRoute::new(
            instance_id,
            1,
            "127.0.0.1".parse().unwrap(),
            9,
            [("GET", "/health")],
        )
        .unwrap();
        let request = route.request("GET", "/health", 0).unwrap();
        let interrupt = super::ReadinessInterrupt::new(
            database.clone(),
            "readiness-exit".into(),
            instance_id.into(),
            crate::spark::executor::ExecutorClient::new(socket),
            1,
        );

        assert!(!tokio::time::timeout(
            Duration::from_millis(100),
            super::wait_until_engine_ready(
                &route,
                &request,
                None,
                Duration::from_secs(5),
                Some(&interrupt),
            ),
        )
        .await
        .expect("container exit must interrupt readiness"));
        database.shutdown().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn disabled_startup_protocol_probe_stops_after_semantic_health() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                let body = if index == 0 {
                    r#"{"data":[{"id":"served-model"}]}"#
                } else {
                    r#"{"id":"cmpl-probe","object":"text_completion","model":"served-model","choices":[{"index":0,"text":"OK","finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let route = crate::spark::upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [
                ("GET", "/v1/models"),
                ("POST", "/v1/completions"),
                ("POST", "/v1/chat/completions"),
            ],
        )
        .unwrap();
        let mut observed: crate::spark::executor::ObservedEngine =
            serde_json::from_value(serde_json::json!({
                "instance_id":"i_11111111111111111111111111111111","generation":1,
                "container_id":"container","network_id":"network","address":address.ip().to_string(),"port":address.port(),
                "running":true,"restart_policy":"no","health_method":"GET","health_path":"/health",
                "allowed_routes":[],"gateway_profile":crate::spark::gateway::GatewayProfile::text(),
                "served_model":"served-model","semantic_prompt":"probe","semantic_max_tokens":1,
                "startup_deadline_seconds":5,"init_pid":1,"pid_start_time_ticks":1,"cgroup_path":"/fixture"
            }))
            .unwrap();
        observed.gateway_profile.startup_protocol_probe = false;
        assert!(
            super::semantic_probe_result(&route, &observed, Duration::from_secs(1))
                .await
                .is_ok()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn embedding_only_route_rewrites_identity_and_denies_generation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let body = r#"{"object":"list","model":"Qwen3-Embedding-0.6B","data":[{"object":"embedding","index":0,"embedding":[1.0,0.0]}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let upstream = crate::spark::upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [("POST", "/v1/embeddings")],
        )
        .unwrap();
        state.routes.publish_with_profile(
            "embeddings",
            "qwen3-embedding:0.6b".into(),
            "Qwen3-Embedding-0.6B".into(),
            crate::spark::gateway::GatewayProfile::embedding(2, 1, 32, true, 1_000),
            upstream,
        );
        let request = Request::post("/openai/embeddings/v1/embeddings")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(r#"{"model":"public","input":"hello"}"#))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["model"],
            "qwen3-embedding:0.6b"
        );
        let generation = Request::post("/openai/embeddings/v1/responses")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(r#"{"input":"hello"}"#))
            .unwrap();
        assert_eq!(
            router(state).oneshot(generation).await.unwrap().status(),
            axum::http::StatusCode::NOT_FOUND
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn alternate_engine_routes_are_not_public() {
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        for path in ["health", "metrics", "tokenize", "../health"] {
            let request = Request::get(format!("/openai/ornith/v1/{path}"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap();
            let response = router(state.clone()).oneshot(request).await.unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["code"],
                "route_not_found"
            );
        }
    }

    #[tokio::test]
    async fn anthropic_x_api_key_is_the_same_scoped_token_presentation() {
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let missing = Request::post("/anthropic/missing/v1/messages")
            .header("anthropic-version", "2023-06-01")
            .body(Body::from("{}"))
            .unwrap();
        let response = router(state.clone()).oneshot(missing).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["type"], "authentication_error");
        let request = Request::post("/anthropic/missing/v1/messages")
            .header("x-api-key", TOKEN)
            .header("anthropic-version", "2023-06-01")
            .body(Body::from(
                r#"{"model":"ornith","messages":[{"role":"user","content":"hi"}],"max_tokens":8}"#,
            ))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn anthropic_unknown_beta_and_engine_routes_fail_closed() {
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let beta = Request::post("/anthropic/missing/v1/messages?beta=true")
            .header("x-api-key", TOKEN)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "unreviewed-feature-2099-01-01")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            router(state.clone()).oneshot(beta).await.unwrap().status(),
            axum::http::StatusCode::BAD_REQUEST
        );
        let hidden = Request::get("/anthropic/missing/v1/models")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router(state).oneshot(hidden).await.unwrap().status(),
            axum::http::StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn headers_limits_auth_and_generation_are_enforced_before_upstream() {
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let unauthenticated = Request::post("/openai/ornith/v1/responses")
            .body(Body::from(r#"{"input":"x"}"#))
            .unwrap();
        assert_eq!(
            router(state.clone())
                .oneshot(unauthenticated)
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
        let forwarded = Request::post("/openai/ornith/v1/responses")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("x-forwarded-for", "127.0.0.1")
            .body(Body::from(r#"{"input":"x"}"#))
            .unwrap();
        assert_eq!(
            router(state.clone())
                .oneshot(forwarded)
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::BAD_REQUEST
        );
        let observed = crate::spark::upstream::ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            "172.30.0.2".parse().unwrap(),
            8000,
            [("POST", "/v1/chat/completions")],
        )
        .unwrap();
        state.routes.publish(
            "ornith",
            "ornith".into(),
            "Ornith-1.5-9B".into(),
            observed.clone(),
        );
        state.routes.drain("ornith", 1);
        let stale = Request::post("/openai/ornith/v1/responses")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(r#"{"input":"x"}"#))
            .unwrap();
        assert_eq!(
            router(state.clone()).oneshot(stale).await.unwrap().status(),
            axum::http::StatusCode::NOT_FOUND
        );
        state
            .routes
            .publish("ornith", "ornith".into(), "Ornith-1.5-9B".into(), observed);
        let hosted = Request::post("/openai/ornith/v1/responses")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(
                r#"{"input":"x","tools":[{"type":"web_search"}]}"#,
            ))
            .unwrap();
        let response = router(state).oneshot(hosted).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn inference_token_concurrency_is_independent_and_released() {
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        let slot = state.inference_slot("token-a", 1);
        let permit = Arc::clone(&slot).acquire_owned().await.unwrap();
        assert!(Arc::clone(&slot).try_acquire_owned().is_err());
        assert!(state
            .inference_slot("token-b", 1)
            .try_acquire_owned()
            .is_ok());
        drop(permit);
        assert!(slot.try_acquire_owned().is_ok());
    }

    #[test]
    fn only_live_changed_event_epoch_triggers_prompt_full_scan() {
        let mut health = crate::spark::wire::ExecutorHealth {
            schema: "sy.spark.executor.health/v1".into(),
            version: "test".into(),
            authorized_agent_uid: 996,
            guard_heartbeat: true,
            event_heartbeat: true,
            event_epoch: 2,
        };
        assert_eq!(super::changed_event_epoch(1, &health), Some(2));
        health.event_heartbeat = false;
        assert_eq!(super::changed_event_epoch(1, &health), None);
    }

    #[test]
    fn semantic_probe_timeout_never_exceeds_remaining_startup_budget() {
        assert_eq!(
            super::semantic_probe_timeout(Duration::from_secs(900), Duration::from_secs(850)),
            Duration::from_secs(50)
        );
        assert_eq!(
            super::semantic_probe_timeout(Duration::from_secs(900), Duration::from_secs(1)),
            super::super::upstream::MAX_SEMANTIC_PROBE_TIMEOUT
        );
    }

    #[test]
    fn semantic_probe_diagnostic_is_persistable_without_engine_content() {
        assert_eq!(
            super::semantic_failure_detail("vision probe rejected with HTTP 4xx"),
            "engine semantic capability contract failed: vision probe rejected with HTTP 4xx"
        );
    }

    #[test]
    fn failed_serve_preserves_bounded_final_engine_logs() {
        let detail = super::failure_detail_with_logs(
            "engine health check failed",
            &["loading weights".into(), "CUDA allocation failed".into()],
        );

        assert_eq!(
            detail,
            "engine health check failed; final engine log: loading weights | CUDA allocation failed"
        );
        assert!(
            super::failure_detail_with_logs("failed", &["x".repeat(4_096)])
                .chars()
                .count()
                <= 2_048
        );
    }

    #[test]
    fn warm_candidate_disk_growth_counts_only_unallocated_capacity() {
        let resources = crate::spark::wire::RecipeResourceEnvelopeDocument {
            image_bytes: 20,
            startup_peak_bytes: 30,
            steady_peak_bytes: 25,
            compile_cache_bytes: 100,
        };
        let storage = crate::spark::executor::CandidateStorage {
            image_present: true,
            compile_cache_allocated_bytes: 80,
            compile_cache_namespace: "opaque-cache-namespace".into(),
        };
        assert_eq!(super::required_disk_growth(&resources, &storage), 20);
    }

    struct ResourceExecutor;

    struct StoppedResourceExecutor;

    impl sy_ipc::Handler for StoppedResourceExecutor {
        async fn handle(&self, request: sy_ipc::Request) -> sy_ipc::Response {
            if request.params["action"].get("inspect_instance").is_some() {
                return sy_ipc::Response::Ok {
                    schema_version: sy_ipc::SCHEMA_VERSION,
                    request_id: request.request_id,
                    result: serde_json::json!({"action":"inspect_instance","running":false}),
                    blob: None,
                };
            }
            ResourceExecutor.handle(request).await
        }
    }

    impl sy_ipc::Handler for ResourceExecutor {
        async fn handle(&self, request: sy_ipc::Request) -> sy_ipc::Response {
            let action = &request.params["action"];
            let result = if action == "health" {
                serde_json::json!({"action":"health","health":{"schema":"sy.spark.executor.health/v1","version":"test","authorized_agent_uid":996,"guard_heartbeat":true,"event_heartbeat":true}})
            } else if action == "inspect_protected_host" {
                serde_json::json!({"action":"inspect_protected_host","host":{"schema":"sy.spark.executor.host/v1","hostname":"spark-test","kernel_release":"6.17-test","architecture":"aarch64","identity_sha256":"a".repeat(64)}})
            } else if action == "inspect_docker_version" {
                serde_json::json!({"action":"inspect_docker_version","docker":{"schema":"sy.spark.executor.docker/v1","transport":"unix","version":"29.2.1","api_version":"1.53","minimum_api_version":"1.44","os":"linux","architecture":"aarch64","experimental":false}})
            } else if action == "inspect_resources" {
                serde_json::json!({"action":"inspect_resources","snapshot":{"schema":"sy.spark.resources.snapshot/v1","observed_at_unix_ms":crate::spark::resources::unix_millis(),"mem_total_bytes":127775277056_u64,"mem_available_bytes":107374182400_u64,"memory_full_psi_avg10_percent":0.0,"swap_in_pages_delta":0,"disk_available_bytes":700_u64*1024*1024*1024},"policy":{"system_reserve_bytes":8_u64*1024*1024*1024,"emergency_available_floor_bytes":8_u64*1024*1024*1024,"disk_reserve_bytes":100_u64*1024*1024*1024,"startup_guard_interval_ms":500,"steady_guard_interval_ms":2000,"emergency_consecutive_samples":3,"memory_full_psi_avg10_percent":2.0}})
            } else if action.get("inspect_candidate_storage").is_some() {
                serde_json::json!({"action":"inspect_candidate_storage","storage":{"image_present":true,"compile_cache_allocated_bytes":0,"compile_cache_namespace":"opaque-authoritative-cache-namespace"}})
            } else if action == "inspect_emergency_records" {
                serde_json::json!({"action":"inspect_emergency_records","records":[]})
            } else if action.get("inspect_instance").is_some() {
                serde_json::json!({"action":"inspect_instance","running":null})
            } else if let Some(expected) = action.get("reconcile_scan") {
                serde_json::json!({"action":"reconcile_scan","scan":{"matched":[],"missing":expected,"quarantined":[]}})
            } else {
                let host = crate::spark::recipe::RecipeHost {
                    architecture: "aarch64".into(),
                    gpu_model: "NVIDIA GB10".into(),
                    compute_capability: "12.1".into(),
                    dgx_build: "7.5.0".into(),
                    driver_version: "580.159.03".into(),
                    toolkit_version: "1.19.0".into(),
                    protected_fingerprint:
                        "sha256:7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510"
                            .into(),
                };
                let catalog = crate::spark::recipe::RecipeCatalog::signed_for_test().query(
                    &host,
                    Some("ornith-ai/Ornith-1.5-9B"),
                    Some("489cb97981b8654bcfcf30ce1f94ed1b62e07b53"),
                    "agent",
                    chrono::Utc::now(),
                );
                serde_json::json!({"action":"inspect_recipes","catalog":catalog})
            };
            sy_ipc::Response::Ok {
                schema_version: sy_ipc::SCHEMA_VERSION,
                request_id: request.request_id,
                result,
                blob: None,
            }
        }
    }

    struct CountingReconcileExecutor {
        scans: Arc<AtomicUsize>,
        prepares: Arc<AtomicUsize>,
        prepared: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    }

    impl sy_ipc::Handler for CountingReconcileExecutor {
        async fn handle(&self, request: sy_ipc::Request) -> sy_ipc::Response {
            let action = &request.params["action"];
            if action.get("reconcile_scan").is_some() {
                self.scans.fetch_add(1, Ordering::SeqCst);
            }
            if action.get("prepare_instance").is_some() {
                self.prepares.fetch_add(1, Ordering::SeqCst);
                self.prepared
                    .lock()
                    .unwrap()
                    .push(action["prepare_instance"].clone());
                return sy_ipc::Response::Ok {
                    schema_version: sy_ipc::SCHEMA_VERSION,
                    request_id: request.request_id,
                    result: serde_json::json!({"action":"prepare_instance","startup_deadline_seconds":900}),
                    blob: None,
                };
            }
            if action.get("start_instance").is_some() {
                return sy_ipc::Response::Ok {
                    schema_version: sy_ipc::SCHEMA_VERSION,
                    request_id: request.request_id,
                    result: serde_json::json!({"action":"stop_instance"}),
                    blob: None,
                };
            }
            ResourceExecutor.handle(request).await
        }
    }

    struct MatchedReconcileExecutor {
        mutations: Arc<AtomicUsize>,
    }

    struct StopRaceExecutor {
        stops: Arc<AtomicUsize>,
        removed: Arc<std::sync::atomic::AtomicBool>,
        fail_after_removal: bool,
        fail_while_stopping: bool,
        stopping_inspections: Arc<AtomicUsize>,
    }

    impl sy_ipc::Handler for StopRaceExecutor {
        async fn handle(&self, request: sy_ipc::Request) -> sy_ipc::Response {
            let action = &request.params["action"];
            let result = if let Some(expected) = action.get("reconcile_scan") {
                let matched = if self.removed.load(Ordering::SeqCst)
                    || expected.as_array().is_none_or(Vec::is_empty)
                {
                    Vec::new()
                } else {
                    let identity = &expected[0];
                    vec![serde_json::json!({
                        "instance_id":identity["instance_id"],"generation":identity["generation"],
                        "container_id":"container-g1","network_id":"network","address":"172.30.0.2","port":8000,
                        "running":true,"restart_policy":"unless-stopped","health_method":"GET","health_path":"/health",
                        "allowed_routes":[["GET","/health"]],"gateway_profile":crate::spark::gateway::GatewayProfile::text(),
                        "served_model":"Ornith-1.5-9B","semantic_prompt":"health","semantic_max_tokens":1,
                        "startup_deadline_seconds":900,"init_pid":1,"pid_start_time_ticks":1,
                        "cgroup_path":"/system.slice/docker-container-g1.scope"
                    })]
                };
                serde_json::json!({"action":"reconcile_scan","scan":{
                    "matched":matched,"missing":[],"quarantined":[]}})
            } else if action.get("stop_instance").is_some() {
                let attempt = self.stops.fetch_add(1, Ordering::SeqCst);
                if self.fail_while_stopping && attempt == 0 {
                    return sy_ipc::Response::Err {
                        schema_version: sy_ipc::SCHEMA_VERSION,
                        request_id: request.request_id,
                        error: sy_ipc::ErrorBody {
                            code: sy_core::ErrorCode::Internal,
                            message: "stop still completing".into(),
                            retry_after_ms: None,
                            details: serde_json::Value::Null,
                        },
                    };
                }
                self.removed.store(true, Ordering::SeqCst);
                if self.fail_after_removal {
                    return sy_ipc::Response::Err {
                        schema_version: sy_ipc::SCHEMA_VERSION,
                        request_id: request.request_id,
                        error: sy_ipc::ErrorBody {
                            code: sy_core::ErrorCode::Internal,
                            message: "late remove race".into(),
                            retry_after_ms: None,
                            details: serde_json::Value::Null,
                        },
                    };
                }
                serde_json::json!({"action":"stop_instance"})
            } else if action.get("inspect_instance").is_some() && self.fail_while_stopping {
                let inspection = self.stopping_inspections.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({"action":"inspect_instance","running":inspection == 0})
            } else {
                return ResourceExecutor.handle(request).await;
            };
            sy_ipc::Response::Ok {
                schema_version: sy_ipc::SCHEMA_VERSION,
                request_id: request.request_id,
                result,
                blob: None,
            }
        }
    }

    impl sy_ipc::Handler for MatchedReconcileExecutor {
        async fn handle(&self, request: sy_ipc::Request) -> sy_ipc::Response {
            let action = &request.params["action"];
            let result = if let Some(expected) = action.get("reconcile_scan") {
                let identity = &expected[0];
                serde_json::json!({"action":"reconcile_scan","scan":{
                    "matched":[{
                        "instance_id":identity["instance_id"],"generation":identity["generation"],
                        "container_id":"container-g7","network_id":"network","address":"172.30.0.2","port":8000,
                        "running":false,"restart_policy":"no","health_method":"GET","health_path":"/health",
                        "allowed_routes":[["GET","/health"],["GET","/v1/models"],["POST","/v1/completions"]],
                        "gateway_profile":crate::spark::gateway::GatewayProfile::text(),
                        "served_model":"Ornith-1.5-9B","semantic_prompt":"Generate one completion token.",
                        "semantic_max_tokens":1,"startup_deadline_seconds":900,"init_pid":1,
                        "pid_start_time_ticks":1,"cgroup_path":"/system.slice/docker-container-g7.scope"
                    }],"missing":[],"quarantined":[]}})
            } else {
                if action.get("disable_restart_policy").is_some()
                    || action.get("stop_instance").is_some()
                {
                    self.mutations.fetch_add(1, Ordering::SeqCst);
                }
                return ResourceExecutor.handle(request).await;
            };
            sy_ipc::Response::Ok {
                schema_version: sy_ipc::SCHEMA_VERSION,
                request_id: request.request_id,
                result,
                blob: None,
            }
        }
    }

    fn test_artifacts() -> crate::spark::wire::ModelArtifactsDocument {
        serde_json::from_str(r#"{"schema":"sy.spark.model-artifacts/v2","format":"safetensors","primary":{"path":"model.safetensors","bytes":8,"sha256":null},"auxiliary":[],"quantization":"FP8","capabilities":["text_generation"],"configured_alias":null}"#).unwrap()
    }

    fn ornith_model() -> crate::spark::wire::ModelDocument {
        crate::spark::wire::ModelDocument {
            schema: crate::spark::wire::MODEL_SCHEMA.into(),
            id: "m_0123456789abcdef0123456789abcdef".into(),
            canonical: "huggingface:ornith-ai/Ornith-1.5-9B@489cb97981b8654bcfcf30ce1f94ed1b62e07b53".into(),
            repository: "ornith-ai/Ornith-1.5-9B".into(),
            commit: "489cb97981b8654bcfcf30ce1f94ed1b62e07b53".into(),
            snapshot: "models--ornith-ai--Ornith-1.5-9B/snapshots/489cb97981b8654bcfcf30ce1f94ed1b62e07b53".into(),
            artifacts: Some(test_artifacts()),
            logical_bytes: 1,
            unique_bytes: 1,
            aliases: vec!["ornith-1.5:9b".into()],
            active_instances: Vec::new(),
            transport: "fixture".into(),
            verified_at: "2026-08-24T00:00:00Z".into(),
            gated: false,
            license: Some("mit".into()),
        }
    }

    fn creating_instance(
        model: &crate::spark::wire::ModelDocument,
    ) -> crate::spark::wire::InstanceDocument {
        let artifacts = model.artifacts.clone().unwrap();
        crate::spark::wire::InstanceDocument {
            schema: crate::spark::wire::INSTANCE_SCHEMA.into(),
            id: format!("i_{}", "1".repeat(32)),
            name: "ornith".into(),
            model_id: model.id.clone(),
            model: model.canonical.clone(),
            model_commit: model.commit.clone(),
            engine_id: "vllm-arm64".into(),
            engine_fingerprint: format!("sha256:{}", "b".repeat(64)),
            artifact_fingerprint: crate::spark::engine::artifact_fingerprint(&artifacts).unwrap(),
            artifacts,
            objective: "agent".into(),
            resources: crate::spark::wire::RecipeResourceEnvelopeDocument {
                image_bytes: 1,
                startup_peak_bytes: 2,
                steady_peak_bytes: 1,
                compile_cache_bytes: 1,
            },
            context_window: 65_536,
            default_reasoning_effort: None,
            generation: 0,
            desired: crate::spark::wire::InstanceDesiredState::Running,
            observed: crate::spark::wire::InstanceObservedState::Creating,
            endpoint: None,
            healthy: false,
            started_at: None,
            startup_milliseconds: None,
            last_failure: None,
            restart_failures: 0,
            restart_suppressed: false,
            quarantine: None,
        }
    }

    #[test]
    fn published_exact_generation_needs_no_recovery_probe() {
        let mut instance = creating_instance(&ornith_model());
        instance.generation = 1;
        let routes = crate::spark::gateway::RouteRegistry::default();
        let upstream = crate::spark::upstream::ObservedRoute::new(
            &instance.id,
            instance.generation,
            "172.30.0.2".parse().unwrap(),
            8000,
            [("GET", "/health")],
        )
        .unwrap();
        routes.publish(
            &instance.name,
            instance.model.clone(),
            "Ornith-1.5-9B".into(),
            upstream,
        );

        assert!(super::exact_route_is_published(&routes, &instance));
        instance.generation += 1;
        assert!(!super::exact_route_is_published(&routes, &instance));
    }

    #[test]
    fn instance_list_never_advertises_an_unpublished_route_as_healthy() {
        let routes = crate::spark::gateway::RouteRegistry::default();
        let mut instance = creating_instance(&ornith_model());
        instance.generation = 1;
        instance.observed = crate::spark::wire::InstanceObservedState::Healthy;
        instance.healthy = true;
        instance.endpoint = Some("/openai/ornith/v1".into());

        super::project_route_health(&routes, std::slice::from_mut(&mut instance));

        assert_eq!(
            instance.observed,
            crate::spark::wire::InstanceObservedState::Degraded
        );
        assert!(!instance.healthy);
        assert!(instance.endpoint.is_none());
    }

    #[test]
    fn stopped_instances_are_not_executor_reconcile_expectations() {
        let mut instance = creating_instance(&ornith_model());
        instance.desired = crate::spark::wire::InstanceDesiredState::Stopped;
        assert!(!super::reconcile_expects_container(&instance));
    }

    #[tokio::test]
    async fn reconcile_restarts_the_same_engine_and_artifact_generation() {
        let (state, database, root) = durable_state().await;
        let socket = root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let scans = Arc::new(AtomicUsize::new(0));
        let prepares = Arc::new(AtomicUsize::new(0));
        let prepared = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = tokio::spawn(
            sy_ipc::Server::new(CountingReconcileExecutor {
                scans: Arc::clone(&scans),
                prepares: Arc::clone(&prepares),
                prepared: Arc::clone(&prepared),
            })
            .serve(listener),
        );
        let model = ornith_model();
        database.promote_model(model.clone(), false).await.unwrap();
        database
            .accept_operation(
                "bootstrap",
                "instance.serve",
                "01K00000000000000000000000",
                &"a".repeat(64),
                Some("ornith".into()),
            )
            .await
            .unwrap();
        let instance = database
            .begin_serve(creating_instance(&model))
            .await
            .unwrap()
            .instance;
        let executor = crate::spark::executor::ExecutorClient::new(socket);
        let active = state.admission.try_acquire("active-serve").unwrap();

        super::reconcile_once(&database, &executor, None, &state.admission)
            .await
            .unwrap();
        assert_eq!(scans.load(Ordering::SeqCst), 1);
        assert_eq!(prepares.load(Ordering::SeqCst), 0);
        assert_eq!(
            database
                .instance(&instance.id)
                .await
                .unwrap()
                .restart_failures,
            0
        );

        drop(active);
        let fresh = crate::spark::resources::TransitionCoordinator::new();
        super::reconcile_once(&database, &executor, None, &fresh)
            .await
            .unwrap();
        assert_eq!(prepares.load(Ordering::SeqCst), 1);
        let prepared = prepared.lock().unwrap().clone();
        assert_eq!(prepared[0]["engine_id"], instance.engine_id);
        assert_eq!(
            prepared[0]["engine_fingerprint"],
            instance.engine_fingerprint
        );
        assert_eq!(
            prepared[0]["artifact_fingerprint"],
            instance.artifact_fingerprint
        );
        assert_eq!(prepared[0]["generation"], instance.generation);
        assert_eq!(
            database
                .instance(&instance.id)
                .await
                .unwrap()
                .restart_failures,
            1
        );
        database.shutdown().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn matched_event_cannot_complete_an_active_semantic_probe() {
        let (state, database, root) = durable_state().await;
        let socket = root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mutations = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(
            sy_ipc::Server::new(MatchedReconcileExecutor {
                mutations: Arc::clone(&mutations),
            })
            .serve(listener),
        );
        let model = ornith_model();
        database.promote_model(model.clone(), false).await.unwrap();
        let operation = database
            .accept_operation(
                "bootstrap",
                "instance.serve",
                "01K00000000000000000000000",
                &"a".repeat(64),
                Some("ornith".into()),
            )
            .await
            .unwrap()
            .operation;
        let instance = database
            .begin_serve(creating_instance(&model))
            .await
            .unwrap()
            .instance;
        database
            .transition(
                &operation.id,
                crate::spark::wire::OperationState::Running,
                super::lifecycle_progress("warming", "semantic probe is active"),
                None,
                None,
            )
            .await
            .unwrap();
        let active = state
            .admission
            .try_acquire("active-semantic-probe")
            .unwrap();
        let executor = crate::spark::executor::ExecutorClient::new(socket);
        super::reconcile_once(&database, &executor, Some(&state.routes), &state.admission)
            .await
            .unwrap();
        assert_eq!(
            database.operation(&operation.id).await.unwrap().state,
            crate::spark::wire::OperationState::Running
        );
        assert_eq!(
            database
                .instance(&instance.id)
                .await
                .unwrap()
                .restart_failures,
            0
        );
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
        drop(active);
        database.shutdown().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn recovered_serve_cannot_succeed_from_degraded_durable_state() {
        let (_state, database, _root) = durable_state().await;
        let model = ornith_model();
        database.promote_model(model.clone(), false).await.unwrap();
        let operation = database
            .accept_operation(
                "bootstrap",
                "instance.serve",
                "01K00000000000000000000000",
                &"a".repeat(64),
                Some("ornith".into()),
            )
            .await
            .unwrap()
            .operation;
        let instance = database
            .begin_serve(creating_instance(&model))
            .await
            .unwrap()
            .instance;
        database
            .transition(
                &operation.id,
                crate::spark::wire::OperationState::Running,
                super::lifecycle_progress("recovering", "checking exact state"),
                None,
                None,
            )
            .await
            .unwrap();
        let operations = database.list_operations().await.unwrap();
        super::complete_recovered_operation(
            &database,
            &operations,
            &instance,
            None,
            "instance.serve",
        )
        .await;
        assert_eq!(
            database.operation(&operation.id).await.unwrap().state,
            crate::spark::wire::OperationState::Running
        );
        database.shutdown().unwrap();
    }

    #[tokio::test]
    async fn stopped_intent_recovers_over_real_executor_protocol_without_model_removal() {
        let (state, database, root) = durable_state().await;
        let socket = root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let _ = sy_ipc::Server::new(ResourceExecutor).serve(listener).await;
        });
        let model = crate::spark::wire::ModelDocument {
            schema: crate::spark::wire::MODEL_SCHEMA.into(),
            id: "m_0123456789abcdef0123456789abcdef".into(),
            canonical: format!("huggingface:owner/model@{}", "a".repeat(40)),
            repository: "owner/model".into(),
            commit: "a".repeat(40),
            snapshot: format!("models--owner--model/snapshots/{}", "a".repeat(40)),
            artifacts: Some(test_artifacts()),
            logical_bytes: 1,
            unique_bytes: 1,
            aliases: vec!["model:one".into()],
            active_instances: Vec::new(),
            transport: "fixture".into(),
            verified_at: "2026-08-24T00:00:00Z".into(),
            gated: false,
            license: Some("mit".into()),
        };
        database.promote_model(model.clone(), false).await.unwrap();
        let artifacts = model.artifacts.clone().unwrap();
        let instance = crate::spark::wire::InstanceDocument {
            schema: crate::spark::wire::INSTANCE_SCHEMA.into(),
            id: format!("i_{}", "1".repeat(32)),
            name: "fixture".into(),
            model_id: model.id.clone(),
            model: model.canonical.clone(),
            model_commit: model.commit.clone(),
            engine_id: "vllm-arm64".into(),
            engine_fingerprint: format!("sha256:{}", "b".repeat(64)),
            artifact_fingerprint: crate::spark::engine::artifact_fingerprint(&artifacts).unwrap(),
            artifacts,
            objective: "agent".into(),
            resources: crate::spark::wire::RecipeResourceEnvelopeDocument {
                image_bytes: 1,
                startup_peak_bytes: 2,
                steady_peak_bytes: 1,
                compile_cache_bytes: 1,
            },
            context_window: 65_536,
            default_reasoning_effort: None,
            generation: 0,
            desired: crate::spark::wire::InstanceDesiredState::Running,
            observed: crate::spark::wire::InstanceObservedState::Creating,
            endpoint: None,
            healthy: false,
            started_at: None,
            startup_milliseconds: None,
            last_failure: None,
            restart_failures: 0,
            restart_suppressed: false,
            quarantine: None,
        };
        let instance = database.begin_serve(instance).await.unwrap().instance;
        database.begin_stop(&instance.id).await.unwrap();
        let executor = crate::spark::executor::ExecutorClient::new(socket);
        super::reconcile_once(&database, &executor, None, &state.admission)
            .await
            .unwrap();
        assert_eq!(
            database.instance(&instance.id).await.unwrap().observed,
            crate::spark::wire::InstanceObservedState::Absent
        );
        assert_eq!(database.model(&model.id).await.unwrap().id, model.id);
        database.shutdown().unwrap();
        server.abort();
        drop(state);
    }

    #[tokio::test]
    async fn explicit_stop_completes_one_removal_while_transition_is_busy() {
        let (state, database, root) = durable_state().await;
        let socket = root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let stops = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(
            sy_ipc::Server::new(StopRaceExecutor {
                stops: Arc::clone(&stops),
                removed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                fail_after_removal: true,
                fail_while_stopping: false,
                stopping_inspections: Arc::new(AtomicUsize::new(0)),
            })
            .serve(listener),
        );
        let model = ornith_model();
        database.promote_model(model.clone(), false).await.unwrap();
        let instance = database
            .begin_serve(creating_instance(&model))
            .await
            .unwrap()
            .instance;
        let _lease = state.admission.try_acquire("explicit-stop").unwrap();
        let operation = database
            .accept_operation(
                "bootstrap",
                "instance.stop",
                "stop-race",
                &"a".repeat(64),
                Some(instance.id.clone()),
            )
            .await
            .unwrap()
            .operation;
        let stopping = database.begin_stop(&instance.id).await.unwrap();
        let executor = crate::spark::executor::ExecutorClient::new(socket);

        super::reconcile_once(&database, &executor, None, &state.admission)
            .await
            .unwrap();
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        super::run_stop(
            database.clone(),
            executor,
            state.routes.clone(),
            operation.id.clone(),
            stopping,
            5,
        )
        .await;

        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert_eq!(
            database.operation(&operation.id).await.unwrap().state,
            crate::spark::wire::OperationState::Succeeded
        );
        database.shutdown().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn explicit_stop_reaps_the_same_generation_after_delayed_shutdown() {
        let (state, database, root) = durable_state().await;
        let socket = root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let stops = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(
            sy_ipc::Server::new(StopRaceExecutor {
                stops: Arc::clone(&stops),
                removed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                fail_after_removal: false,
                fail_while_stopping: true,
                stopping_inspections: Arc::new(AtomicUsize::new(0)),
            })
            .serve(listener),
        );
        let model = ornith_model();
        database.promote_model(model.clone(), false).await.unwrap();
        let instance = database
            .begin_serve(creating_instance(&model))
            .await
            .unwrap()
            .instance;
        let operation = database
            .accept_operation(
                "bootstrap",
                "instance.stop",
                "delayed-stop",
                &"a".repeat(64),
                Some(instance.id.clone()),
            )
            .await
            .unwrap()
            .operation;
        let stopping = database.begin_stop(&instance.id).await.unwrap();

        super::run_stop(
            database.clone(),
            crate::spark::executor::ExecutorClient::new(socket),
            state.routes.clone(),
            operation.id.clone(),
            stopping,
            5,
        )
        .await;

        assert_eq!(stops.load(Ordering::SeqCst), 2);
        assert_eq!(
            database.operation(&operation.id).await.unwrap().state,
            crate::spark::wire::OperationState::Succeeded
        );
        database.shutdown().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn explicit_stop_preempts_a_busy_high_memory_transition() {
        let (state, database, root) = durable_state().await;
        let socket = root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(
            sy_ipc::Server::new(StopRaceExecutor {
                stops: Arc::new(AtomicUsize::new(0)),
                removed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                fail_after_removal: false,
                fail_while_stopping: false,
                stopping_inspections: Arc::new(AtomicUsize::new(0)),
            })
            .serve(listener),
        );
        let state = state.with_executor(crate::spark::executor::ExecutorClient::new(socket));
        let model = ornith_model();
        database.promote_model(model.clone(), false).await.unwrap();
        let instance = database
            .begin_serve(creating_instance(&model))
            .await
            .unwrap()
            .instance;
        let _active = state.admission.try_acquire("active-transition").unwrap();
        let request = Request::delete(format!("{API_BASE}/instances/{}", instance.id))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", "busy-stop")
            .body(Body::from(r#"{"timeout_seconds":5,"dry_run":false}"#))
            .unwrap();

        let response = router(state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
        assert_eq!(database.list_operations().await.unwrap().len(), 1);
        assert_eq!(
            database.instance(&instance.id).await.unwrap().desired,
            crate::spark::wire::InstanceDesiredState::Stopped
        );
        database.shutdown().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn explicit_stop_of_absent_generation_succeeds_without_executor_removal() {
        let (state, database, root) = durable_state().await;
        let model = ornith_model();
        database.promote_model(model.clone(), false).await.unwrap();
        let instance = database
            .begin_serve(creating_instance(&model))
            .await
            .unwrap()
            .instance;
        database
            .set_instance_observed(
                &instance.id,
                instance.generation,
                crate::spark::wire::InstanceObservedState::Absent,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let state = state.with_executor(crate::spark::executor::ExecutorClient::new(
            root.join("unused-executor.sock"),
        ));
        let request = Request::delete(format!("{API_BASE}/instances/{}", instance.id))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", "absent-stop")
            .body(Body::from(r#"{"timeout_seconds":5,"dry_run":false}"#))
            .unwrap();

        let response = router(state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
        let operations = database.list_operations().await.unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(
            operations[0].state,
            crate::spark::wire::OperationState::Succeeded
        );
        assert_eq!(
            database.instance(&instance.id).await.unwrap().observed,
            crate::spark::wire::InstanceObservedState::Absent
        );
        database.shutdown().unwrap();
    }

    #[tokio::test]
    async fn explicit_stop_reaps_quarantined_evidence_even_when_state_is_absent() {
        let (state, database, root) = durable_state().await;
        let socket = root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let stops = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(
            sy_ipc::Server::new(StopRaceExecutor {
                stops: Arc::clone(&stops),
                removed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                fail_after_removal: false,
                fail_while_stopping: false,
                stopping_inspections: Arc::new(AtomicUsize::new(0)),
            })
            .serve(listener),
        );
        let model = ornith_model();
        database.promote_model(model.clone(), false).await.unwrap();
        let instance = database
            .begin_serve(creating_instance(&model))
            .await
            .unwrap()
            .instance;
        database
            .mark_quarantine(&instance.id, instance.generation, "stale-container")
            .await
            .unwrap();
        database
            .set_instance_observed(
                &instance.id,
                instance.generation,
                crate::spark::wire::InstanceObservedState::Absent,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let state = state.with_executor(crate::spark::executor::ExecutorClient::new(socket));
        let request = Request::delete(format!("{API_BASE}/instances/{}", instance.id))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", "quarantined-absent-stop")
            .body(Body::from(r#"{"timeout_seconds":5,"dry_run":false}"#))
            .unwrap();

        let response = router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
        for _ in 0..20 {
            if database.list_operations().await.unwrap()[0]
                .state
                .is_terminal()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert_eq!(
            database.list_operations().await.unwrap()[0].state,
            crate::spark::wire::OperationState::Succeeded
        );
        database.shutdown().unwrap();
        server.abort();
    }

    async fn durable_state() -> (super::AgentState, crate::spark::state::DbActor, PathBuf) {
        let root = tempfile::tempdir().unwrap().keep();
        let database = crate::spark::state::DbActor::open(
            root.join("state.sqlite3"),
            root.join("backups"),
            8,
            7,
            secrecy::SecretString::from(TOKEN),
        )
        .unwrap();
        let state = super::AgentState::new(TOKEN, Vec::new(), Vec::new())
            .with_database(database.clone())
            .await
            .unwrap()
            .with_ready_executor_for_test();
        (state, database, root)
    }

    #[tokio::test]
    async fn dry_run_reports_engine_artifact_and_reserve_without_side_effects() {
        const CACHE_NAMESPACE: &str = "opaque-authoritative-cache-namespace";
        let (state, database, root) = durable_state().await;
        let socket = root.join("executor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let _ = sy_ipc::Server::new(ResourceExecutor).serve(listener).await;
        });
        database
            .promote_model(
                crate::spark::wire::ModelDocument {
                    schema: crate::spark::wire::MODEL_SCHEMA.into(),
                    id: "m_0123456789abcdef0123456789abcdef".into(),
                    canonical: "huggingface:ornith-ai/Ornith-1.5-35B-A3B-GGUF@12393612fd4f730ff5aadc23e9b8f9648aa49ceb".into(),
                    repository: "ornith-ai/Ornith-1.5-35B-A3B-GGUF".into(),
                    commit: "12393612fd4f730ff5aadc23e9b8f9648aa49ceb".into(),
                    snapshot: "models--ornith-ai--Ornith-1.5-35B-A3B-GGUF/snapshots/12393612fd4f730ff5aadc23e9b8f9648aa49ceb".into(),
                    artifacts: Some(crate::spark::wire::ModelArtifactsDocument {
                        schema: "sy.spark.model-artifacts/v2".into(),
                        format: crate::spark::wire::ModelArtifactFormat::Gguf,
                        primary: crate::spark::wire::ModelArtifactFileDocument {
                            path: "Ornith-1.5-35B-Q4_K_M.gguf".into(),
                            bytes: 1,
                            sha256: None,
                        },
                        auxiliary: Vec::new(),
                        quantization: Some("Q4_K_M".into()),
                        capabilities: vec!["text_generation".into()],
                        configured_alias: None,
                        engine_profile: None,
                    }),
                    logical_bytes: 1,
                    unique_bytes: 1,
                    aliases: vec!["ornith-1.5:35b".into()],
                    active_instances: Vec::new(),
                    transport: "rust-xet".into(),
                    verified_at: "2026-08-24T00:00:00Z".into(),
                    gated: false,
                    license: Some("mit".into()),
                },
                false,
            )
            .await
            .unwrap();
        let state = state.with_executor(crate::spark::executor::ExecutorClient::new(socket));
        let request = Request::post(format!("{API_BASE}/admission"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"model":"ornith-1.5:35b","name":null,"dry_run":true}"#,
            ))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let report: crate::spark::resources::AdmissionReport = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(report.admitted);
        let selection = report.selection.unwrap();
        assert_eq!(selection.engine_id, "llama-cpp-cuda13-arm64");
        assert_eq!(selection.selection_kind, "configured_engine");
        assert_eq!(selection.engine, "llama.cpp");
        assert_eq!(
            selection.artifacts.primary.path,
            "Ornith-1.5-35B-Q4_K_M.gguf"
        );
        assert!(selection.artifact_fingerprint.starts_with("sha256:"));
        assert_eq!(report.policy.system_reserve_bytes, 8 * 1024 * 1024 * 1024);
        assert!(selection.image.starts_with("sha256:"));
        assert_eq!(selection.image.len(), 71);
        assert!(selection.fingerprint.starts_with("sha256:"));
        assert_eq!(selection.compile_cache_namespace, CACHE_NAMESPACE);
        server.abort();
        database.shutdown().unwrap();
    }

    #[test]
    fn certificate_status_uses_the_actual_leaf_dns_and_ip_sans() {
        let rcgen::CertifiedKey { cert, .. } = rcgen::generate_simple_self_signed(vec![
            "actual.spark.test".into(),
            "10.1.30.143".into(),
        ])
        .unwrap();
        let status = super::certificate_status_from_pem(cert.pem().as_bytes()).unwrap();
        assert_eq!(status.dns_sans, ["actual.spark.test"]);
        assert_eq!(status.ip_sans, ["10.1.30.143"]);
    }

    #[tokio::test]
    async fn tls_reload_uses_the_explicit_crypto_provider() {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let certificate = directory.path().join("chain.pem");
        let key = directory.path().join("key.pem");
        std::fs::write(&certificate, cert.pem()).unwrap();
        std::fs::write(&key, signing_key.serialize_pem()).unwrap();
        let tls = super::tls13_config(
            std::fs::read(&certificate).unwrap(),
            std::fs::read(&key).unwrap(),
        )
        .await
        .unwrap();
        super::reload_tls_from_pem_files(&tls, &certificate, &key)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn authenticated_status_is_degraded_without_executor() {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = cert.pem();
        let tls = super::tls13_config(
            certificate.as_bytes().to_vec(),
            signing_key.serialize_pem().into_bytes(),
        )
        .await
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = axum_server::Handle::new();
        let server_handle = handle.clone();
        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .unwrap()
                .handle(server_handle)
                .serve(
                    router(AgentState::new(
                        TOKEN,
                        vec!["localhost".into()],
                        vec!["127.0.0.1".into()],
                    ))
                    .into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
        });
        let client = reqwest::Client::builder()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(certificate.as_bytes()).unwrap())
            .build()
            .unwrap();
        let status: crate::spark::wire::StatusDocument = client
            .get(format!(
                "https://localhost:{}/api/sy.spark/v1/status",
                address.port()
            ))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            status.degraded_reasons[0].code,
            "spark.executor.unavailable"
        );
        handle.graceful_shutdown(None);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn executor_loss_preserves_reads_and_rejects_every_mutation_route_class() {
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        for route in ["status", "doctor"] {
            let request = Request::get(format!("{API_BASE}/{route}"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                router(state.clone())
                    .oneshot(request)
                    .await
                    .unwrap()
                    .status(),
                axum::http::StatusCode::OK
            );
        }
        let mutations = [
            Request::delete(format!("{API_BASE}/operations/01HXYZ0000000000000000000Z"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("idempotency-key", "cancel-key")
                .body(Body::empty())
                .unwrap(),
            Request::post(format!("{API_BASE}/tokens"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("idempotency-key", "create-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"reader","scopes":["models:read"],"allowed_cidrs":[],"expires_at":null,"max_concurrent_inference":1}"#,
                ))
                .unwrap(),
            Request::delete(format!("{API_BASE}/tokens/01HXYZ0000000000000000000Z"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("idempotency-key", "revoke-key")
                .body(Body::empty())
                .unwrap(),
            Request::post(format!("{API_BASE}/admission"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"model":"ornith-1.5:9b","name":null,"dry_run":true}"#,
                ))
                .unwrap(),
        ];
        for request in mutations {
            let response = router(state.clone()).oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            );
            let body = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            let problem: crate::spark::wire::ProblemDocument =
                serde_json::from_slice(&body).unwrap();
            assert_eq!(problem.code, "spark.executor.unavailable");
        }
    }

    #[tokio::test]
    async fn unknown_fields_routes_and_cors_are_rejected() {
        let state = AgentState::new(TOKEN, Vec::new(), Vec::new());
        for request in [
            Request::get(format!("{API_BASE}/status?extra=true"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
            Request::get(format!("{API_BASE}/models?extra=true"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
            Request::get(format!("{API_BASE}/status"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::ORIGIN, "https://attacker.invalid"),
        ] {
            let response = router(state.clone())
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(response.status().is_client_error());
            assert_eq!(
                response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
                None
            );
        }
    }

    #[tokio::test]
    async fn same_idempotency_request_reuses_operation_changed_body_conflicts() {
        let (state, database, _root) = durable_state().await;
        let body = serde_json::json!({"name":"reader","scopes":["models:read"],"allowed_cidrs":[],"expires_at":null,"max_concurrent_inference":1});
        let send = |body: serde_json::Value| {
            Request::post(format!("{API_BASE}/tokens"))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("idempotency-key", "same-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };
        let first = router(state.clone())
            .oneshot(send(body.clone()))
            .await
            .unwrap();
        assert_eq!(first.status(), axum::http::StatusCode::ACCEPTED);
        let first: crate::spark::wire::TokenCreatedDocument = serde_json::from_slice(
            &axum::body::to_bytes(first.into_body(), 65536)
                .await
                .unwrap(),
        )
        .unwrap();
        let second = router(state.clone()).oneshot(send(body)).await.unwrap();
        let second: crate::spark::wire::TokenCreatedDocument = serde_json::from_slice(
            &axum::body::to_bytes(second.into_body(), 65536)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first.operation.id, second.operation.id);
        let changed = router(state).oneshot(send(serde_json::json!({"name":"changed","scopes":["models:read"],"allowed_cidrs":[],"expires_at":null,"max_concurrent_inference":1}))).await.unwrap();
        assert_eq!(changed.status(), axum::http::StatusCode::CONFLICT);
        database.shutdown().unwrap();
    }

    #[tokio::test]
    async fn verified_model_inventory_round_trips_over_the_authenticated_wire() {
        let (state, database, _root) = durable_state().await;
        let model = crate::spark::wire::ModelDocument {
            schema: crate::spark::wire::MODEL_SCHEMA.into(),
            id: "m_0123456789abcdef0123456789abcdef".into(),
            canonical: "huggingface:owner/model@0123456789abcdef0123456789abcdef01234567".into(),
            repository: "owner/model".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            snapshot: "models--owner--model/snapshots/0123456789abcdef0123456789abcdef01234567"
                .into(),
            artifacts: None,
            logical_bytes: 1024,
            unique_bytes: 1024,
            aliases: vec!["model:one".into()],
            active_instances: Vec::new(),
            transport: "rust-xet".into(),
            verified_at: "2026-08-24T00:00:00Z".into(),
            gated: false,
            license: Some("mit".into()),
        };
        database.promote_model(model.clone(), false).await.unwrap();
        let request = Request::get(format!("{API_BASE}/models"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let inventory: crate::spark::wire::ModelListDocument =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(inventory.models, vec![model]);
        database.shutdown().unwrap();
    }

    #[tokio::test]
    async fn scoped_token_revocation_is_effective_on_the_next_request() {
        let (state, database, _root) = durable_state().await;
        let created = database
            .create_token(
                "bootstrap-admin",
                "reader-key",
                crate::spark::wire::TokenCreateRequest {
                    name: "status-reader".into(),
                    scopes: vec![crate::spark::wire::TokenScope::InstancesRead],
                    allowed_cidrs: vec!["127.0.0.0/8".into()],
                    expires_at: None,
                    max_concurrent_inference: 1,
                },
            )
            .await
            .unwrap();
        state
            .auth
            .store(std::sync::Arc::new(database.auth_snapshot().await.unwrap()));
        let bearer = created.bearer_token.unwrap();
        let status_request = || {
            let mut request = Request::get(format!("{API_BASE}/status"))
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
            ));
            request
        };
        assert_eq!(
            router(state.clone())
                .oneshot(status_request())
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::OK
        );
        let revoke = Request::delete(format!("{API_BASE}/tokens/{}", created.token.id))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("idempotency-key", "revoke-reader")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router(state.clone())
                .oneshot(revoke)
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::ACCEPTED
        );
        assert_eq!(
            router(state)
                .oneshot(status_request())
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
        database.shutdown().unwrap();
    }

    #[test]
    fn lifecycle_routes_require_the_narrow_scopes() {
        use crate::spark::wire::TokenScope;
        use axum::http::Method;

        assert_eq!(
            super::required_scope(&Method::GET, &format!("{API_BASE}/instances")),
            Some(TokenScope::InstancesRead)
        );
        assert_eq!(
            super::required_scope(&Method::GET, &format!("{API_BASE}/instances/i_1/logs")),
            Some(TokenScope::LogsRead)
        );
        assert_eq!(
            super::required_scope(&Method::POST, &format!("{API_BASE}/instances")),
            Some(TokenScope::InstancesWrite)
        );
        assert_eq!(
            super::required_scope(&Method::DELETE, &format!("{API_BASE}/instances/i_1")),
            Some(TokenScope::InstancesWrite)
        );
        assert_eq!(
            super::required_scope(&Method::POST, &format!("{API_BASE}/benchmarks")),
            None
        );
        assert_eq!(
            super::required_scope(&Method::POST, &format!("{API_BASE}/tunings")),
            None
        );
    }

    #[test]
    fn doctor_discloses_shared_bridge_peer_lateral_risk() {
        let check = super::peer_lateral_risk_check();
        assert_eq!(check.code, "spark.network.peer-lateral-risk");
        assert_eq!(check.status, "accepted-risk");
        assert!(check.detail.contains("reach other managed engine peers"));
    }

    #[test]
    fn generated_openapi_matches_the_normalized_fixture() {
        let document = super::ApiDoc::openapi();
        let value = serde_json::to_value(document).unwrap();
        let mut paths = value["paths"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();
        let mut schemas = value["components"]["schemas"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        schemas.sort();
        let normalized =
            serde_json::json!({"openapi":value["openapi"],"paths":paths,"schemas":schemas});
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../specs/openapi/sy-spark-control-v1.json"))
                .unwrap();
        assert_eq!(normalized, fixture);
    }
}
