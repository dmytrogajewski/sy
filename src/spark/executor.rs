//! Root-only, peer-authorized Spark executor boundary.

use std::{
    collections::BTreeMap,
    future::Future,
    io::Read,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bollard::{
    models::{
        ContainerCreateBody, ContainerUpdateBody, DeviceRequest, EndpointSettings, EventMessage,
        EventMessageTypeEnum, HostConfig, ImageInspect, Mount, MountType, NetworkCreateRequest,
        NetworkingConfig, RestartPolicy, RestartPolicyNameEnum,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, EventsOptionsBuilder,
        ListContainersOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
        StopContainerOptionsBuilder,
    },
    Docker, API_DEFAULT_VERSION,
};
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sy_core::{ErrorCode, Priority};
use sy_ipc::{
    dispatch_with_cancel, CallOpts, CancelRegistry, Dispatched, ErrorBody, Handler, PeerAuthorizer,
    PeerCredentials, Request, Response, Server, SCHEMA_VERSION,
};
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::{
    engine::EnginePolicy,
    gateway::GatewayProfile,
    recipe::{Accelerator, RecipeCatalog, RecipeHost},
    wire::RecipeCatalogDocument,
};
use super::{
    resources::{
        HostResourceSnapshot, HostSampler, ProcfsHostSampler, ResourcePolicy, ResourcePolicyConfig,
    },
    wire::{DockerCapability, ExecutorHealth, ExecutorSnapshot, ProtectedHostSnapshot},
};

pub const EXECUTOR_SOCKET: &str = "/run/sy-spark/executor.sock";
const RECIPE_CATALOG_DIR: &str = "/etc/sy/spark-recipes.d";
const RESOURCE_POLICY_PATH: &str = "/etc/sy/spark-agent.toml";
const ENGINE_POLICY_PATH: &str = "/etc/sy/spark/engine.toml";
#[cfg(test)]
const COMPILE_CACHE_IDENTITY_SCHEMA: &str = "sy.spark.compile-cache-identity/v1";
const EXECUTOR_METHOD: &str = "spark.executor.execute";
const MAX_REQUEST_FRAME: usize = 16 * 1024;
const MAX_DEADLINE_MS: u64 = super::recipe::MAX_STARTUP_DEADLINE_SECONDS * 1_000;
const DOCKER_TIMEOUT_SECONDS: u64 = 3;
const HEARTBEAT_STALE_SECONDS: u64 = 5;
const MAX_ENGINE_LOG_LINES: usize = 256;
const MAX_MANAGED_CONTAINERS: usize = 256;
const MAX_MANAGED_EVENTS_PER_WINDOW: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartInstanceInput {
    pub instance_id: String,
    pub generation: u64,
    pub model_commit: String,
    pub model_repository: String,
    pub recipe_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopInstanceInput {
    pub instance_id: String,
    pub generation: u64,
    pub grace_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogInput {
    pub instance_id: String,
    pub generation: u64,
    pub cursor: u64,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerMount {
    source: String,
    target: String,
    read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerSpec {
    name: String,
    image: String,
    entrypoint: Vec<String>,
    argv: Vec<String>,
    environment: Vec<String>,
    labels: std::collections::BTreeMap<String, String>,
    network: String,
    port: u16,
    health_method: String,
    health_path: String,
    allowed_routes: Vec<(String, String)>,
    gateway_profile: GatewayProfile,
    served_model: String,
    semantic_prompt: String,
    semantic_max_tokens: u32,
    startup_deadline_seconds: u64,
    mounts: Vec<ContainerMount>,
    model_cache_root: String,
    compile_cache_root: String,
    tmpfs: Vec<String>,
    run_as_uid: u32,
    pid_limit: u32,
    memory_bytes: u64,
    read_only_rootfs: bool,
    cap_drop: Vec<String>,
    seccomp: String,
    no_new_privileges: bool,
    published_ports: Vec<u16>,
    restart: String,
    accelerator: Accelerator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedEngine {
    pub instance_id: String,
    pub generation: u64,
    pub container_id: String,
    pub network_id: String,
    pub address: String,
    pub port: u16,
    pub running: bool,
    pub restart_policy: String,
    pub health_method: String,
    pub health_path: String,
    pub allowed_routes: Vec<(String, String)>,
    pub gateway_profile: GatewayProfile,
    pub served_model: String,
    pub semantic_prompt: String,
    pub semantic_max_tokens: u32,
    pub startup_deadline_seconds: u64,
    pub init_pid: u32,
    pub pid_start_time_ticks: u64,
    pub cgroup_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineLogs {
    pub cursor: u64,
    pub next_cursor: u64,
    pub truncated: bool,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedContainerObservation {
    pub container_id: String,
    pub name: String,
    pub instance_id: Option<String>,
    pub generation: Option<u64>,
    pub role: Option<String>,
    pub model_commit: Option<String>,
    pub model_repository: Option<String>,
    pub recipe_id: Option<String>,
    pub image: Option<String>,
    pub networks: Vec<String>,
    pub restart_policy: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileExpectation {
    pub instance_id: String,
    pub generation: u64,
    pub model_commit: String,
    pub model_repository: String,
    pub recipe_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantinedContainer {
    pub container_id: String,
    pub instance_id: Option<String>,
    pub generation: Option<u64>,
    pub cause: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileScan {
    pub matched: Vec<ObservedEngine>,
    pub missing: Vec<ReconcileExpectation>,
    pub quarantined: Vec<QuarantinedContainer>,
}

#[cfg(test)]
fn build_container_spec(
    recipe: &super::recipe::Recipe,
    input: &StartInstanceInput,
    network: &str,
) -> Result<ContainerSpec, ()> {
    if input.instance_id.len() != 34
        || !input.instance_id.starts_with("i_")
        || input.generation == 0
        || input.model_commit.len() != 40
        || input.recipe_id != recipe.identity.id
        || !recipe.model.commits.contains(&input.model_commit)
        || input.operation_id.len() != 26
    {
        return Err(());
    }
    let repository_cache = format!("models--{}", recipe.model.repository.replace('/', "--"));
    let model_snapshot = format!("/models/snapshots/{}", input.model_commit);
    let port = recipe.gateway.port.to_string();
    let context = recipe.resources.context_ceiling.to_string();
    let compile_cache_key = recipe_compile_cache_key(recipe, &input.model_commit)?;
    let substitute = |token: &str| {
        token
            .replace("{model_snapshot}", &model_snapshot)
            .replace("{port}", &port)
            .replace("{max_model_len}", &context)
            .replace("{instance_id}", &input.instance_id)
            .replace("{compile_cache}", "/compile-cache")
    };
    let mounts = recipe
        .isolation
        .mounts
        .iter()
        .map(|mount| {
            let source = match mount.purpose {
                super::recipe::MountPurpose::Model => {
                    format!("{}/{repository_cache}", mount.host_root)
                }
                super::recipe::MountPurpose::CompileCache => format!(
                    "{}/{}/{}",
                    mount.host_root, compile_cache_key, input.instance_id
                ),
            };
            ContainerMount {
                source,
                target: mount.container_path.clone(),
                read_only: mount.read_only,
            }
        })
        .collect();
    let labels = [
        ("io.sy.spark.managed", "true".into()),
        ("io.sy.spark.instance", input.instance_id.clone()),
        ("io.sy.spark.generation", input.generation.to_string()),
        ("io.sy.spark.role", "engine".into()),
        ("io.sy.spark.model_commit", input.model_commit.clone()),
        ("io.sy.spark.recipe", input.recipe_id.clone()),
        ("io.sy.spark.operation", input.operation_id.clone()),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value))
    .collect();
    let model_cache_root = recipe
        .isolation
        .mounts
        .iter()
        .find(|mount| mount.purpose == super::recipe::MountPurpose::Model)
        .map(|mount| mount.host_root.clone())
        .ok_or(())?;
    let compile_cache_root = recipe
        .isolation
        .mounts
        .iter()
        .find(|mount| mount.purpose == super::recipe::MountPurpose::CompileCache)
        .map(|mount| mount.host_root.clone())
        .ok_or(())?;
    Ok(ContainerSpec {
        name: format!("sy-spark-{}-g{}", input.instance_id, input.generation),
        image: format!(
            "{}@{}",
            recipe.engine.image_repository, recipe.engine.image_digest
        ),
        entrypoint: recipe
            .engine
            .entrypoint
            .iter()
            .map(|value| substitute(value))
            .collect(),
        argv: recipe
            .engine
            .argv
            .iter()
            .map(|value| substitute(value))
            .collect(),
        environment: Vec::new(),
        labels,
        network: network.into(),
        port: recipe.gateway.port,
        health_method: recipe.health.method.clone(),
        health_path: recipe.health.path.clone(),
        allowed_routes: std::iter::once((recipe.health.method.clone(), recipe.health.path.clone()))
            .chain(
                recipe
                    .gateway
                    .methods
                    .iter()
                    .map(|route| (route.method.clone(), route.path.clone())),
            )
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
        gateway_profile: recipe.gateway.profile(),
        served_model: recipe.health.served_model.clone(),
        semantic_prompt: recipe.health.semantic_prompt.clone(),
        semantic_max_tokens: recipe.health.semantic_max_tokens,
        startup_deadline_seconds: recipe.health.startup_deadline_seconds,
        mounts,
        model_cache_root,
        compile_cache_root,
        tmpfs: recipe.isolation.writable_tmpfs.clone(),
        run_as_uid: recipe.isolation.run_as_uid,
        pid_limit: recipe.isolation.pid_limit,
        memory_bytes: recipe.resources.startup_peak_bytes,
        read_only_rootfs: true,
        cap_drop: vec!["ALL".into()],
        seccomp: recipe.isolation.seccomp.clone(),
        no_new_privileges: true,
        published_ports: Vec::new(),
        restart: "no".into(),
        accelerator: recipe.engine.accelerator,
    })
}

fn build_generic_container_spec(
    policy: &EnginePolicy,
    input: &StartInstanceInput,
) -> Result<ContainerSpec, ()> {
    let config = policy.config();
    let repository = valid_model_repository(&input.model_repository)?;
    if input.instance_id.len() != 34
        || !input.instance_id.starts_with("i_")
        || input.generation == 0
        || !valid_commit(&input.model_commit)
        || input.recipe_id != config.id
        || input.operation_id.len() != 26
    {
        return Err(());
    }
    let repository_cache = format!("models--{}", input.model_repository.replace('/', "--"));
    let model_snapshot = format!("/models/snapshots/{}", input.model_commit);
    let served_model = repository.1;
    let model_type = read_model_type(config, &input.model_repository, &input.model_commit)
        .ok()
        .flatten();
    let profile = policy.profile(model_type.as_deref());
    let substitute = |value: &str| match value {
        "{model_snapshot}" => model_snapshot.clone(),
        "{served_model}" => served_model.to_owned(),
        "{port}" => config.port.to_string(),
        _ => value.to_owned(),
    };
    let compile_identity = format!(
        "{}\0{}\0{}\0{}",
        policy.fingerprint(),
        input.model_repository,
        input.model_commit,
        profile.id
    );
    let compile_cache_key = format!("sha256-{:x}", Sha256::digest(compile_identity));
    let labels = [
        ("io.sy.spark.managed", "true".into()),
        ("io.sy.spark.instance", input.instance_id.clone()),
        ("io.sy.spark.generation", input.generation.to_string()),
        ("io.sy.spark.role", "engine".into()),
        ("io.sy.spark.model_commit", input.model_commit.clone()),
        (
            "io.sy.spark.model_repository",
            input.model_repository.clone(),
        ),
        ("io.sy.spark.recipe", input.recipe_id.clone()),
        ("io.sy.spark.operation", input.operation_id.clone()),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value))
    .collect();
    Ok(ContainerSpec {
        name: format!("sy-spark-{}-g{}", input.instance_id, input.generation),
        image: policy.image(),
        entrypoint: config
            .entrypoint
            .iter()
            .map(|value| substitute(value))
            .collect(),
        argv: config
            .arguments
            .iter()
            .chain(&profile.arguments)
            .map(|value| substitute(value))
            .collect(),
        environment: config.environment.clone(),
        labels,
        network: config.network.clone(),
        port: config.port,
        health_method: config.health_method.clone(),
        health_path: config.health_path.clone(),
        allowed_routes: std::iter::once((config.health_method.clone(), config.health_path.clone()))
            .chain(
                config
                    .routes
                    .iter()
                    .map(|route| (route.method.clone(), route.path.clone())),
            )
            .collect(),
        gateway_profile: policy.gateway_profile(model_type.as_deref()),
        served_model: served_model.to_owned(),
        semantic_prompt: config.semantic_prompt.clone(),
        semantic_max_tokens: config.semantic_max_tokens,
        startup_deadline_seconds: config.startup_deadline_seconds,
        mounts: vec![
            ContainerMount {
                source: format!("{}/{}", config.model_cache_root, repository_cache),
                target: "/models".into(),
                read_only: true,
            },
            ContainerMount {
                source: format!(
                    "{}/{}/{}",
                    config.compile_cache_root, compile_cache_key, input.instance_id
                ),
                target: "/compile-cache".into(),
                read_only: false,
            },
        ],
        model_cache_root: config.model_cache_root.clone(),
        compile_cache_root: config.compile_cache_root.clone(),
        tmpfs: config.tmpfs.clone(),
        run_as_uid: config.run_as_uid,
        pid_limit: config.pid_limit,
        memory_bytes: config.resources.startup_peak_bytes,
        read_only_rootfs: true,
        cap_drop: vec!["ALL".into()],
        seccomp: config.seccomp.clone(),
        no_new_privileges: true,
        published_ports: Vec::new(),
        restart: "no".into(),
        accelerator: Accelerator::Nvidia,
    })
}

fn valid_model_repository(repository: &str) -> Result<(&str, &str), ()> {
    let (owner, model) = repository.split_once('/').ok_or(())?;
    let valid = |value: &str| {
        !value.is_empty()
            && value.len() <= 96
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            && value != "."
            && value != ".."
    };
    (valid(owner) && valid(model) && !model.contains('/'))
        .then_some((owner, model))
        .ok_or(())
}

fn valid_commit(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn read_model_type(
    config: &super::engine::EngineConfig,
    repository: &str,
    commit: &str,
) -> Result<Option<String>, ()> {
    #[derive(Deserialize)]
    struct ModelConfig {
        model_type: Option<String>,
    }
    let repository_cache = format!("models--{}", repository.replace('/', "--"));
    let path = Path::new(&config.model_cache_root)
        .join(repository_cache)
        .join("snapshots")
        .join(commit)
        .join("config.json");
    let text = std::fs::read_to_string(path).map_err(|_| ())?;
    serde_json::from_str::<ModelConfig>(&text)
        .map(|model| model.model_type)
        .map_err(|_| ())
}

#[cfg(test)]
#[derive(Serialize)]
struct CompileCacheIdentity<'a> {
    schema: &'static str,
    recipe_id: &'a str,
    recipe_version: u32,
    engine: CompileCacheEngineIdentity<'a>,
    model: CompileCacheModelIdentity<'a>,
    model_commit: &'a str,
    context_ceiling: u64,
    image_run_as_uid: u32,
    isolation_run_as_uid: u32,
}

#[cfg(test)]
#[derive(Serialize)]
struct CompileCacheEngineIdentity<'a> {
    name: &'a str,
    version: &'a str,
    image_digest: &'a str,
    image_architecture: &'a str,
    accelerator: Accelerator,
    entrypoint: &'a [String],
    argv: &'a [String],
    substitutions: &'a [super::recipe::Substitution],
}

#[cfg(test)]
#[derive(Serialize)]
struct CompileCacheModelIdentity<'a> {
    repository: &'a str,
    format: &'a str,
    precision: &'a str,
    tokenizer_sha256: &'a str,
    parser: &'a str,
    parser_sha256: &'a str,
    remote_code: &'a super::recipe::RemoteCode,
    files: &'a [super::recipe::RequiredFile],
}

#[cfg(test)]
fn recipe_compile_cache_key(
    recipe: &super::recipe::Recipe,
    model_commit: &str,
) -> Result<String, ()> {
    let identity = CompileCacheIdentity {
        schema: COMPILE_CACHE_IDENTITY_SCHEMA,
        recipe_id: &recipe.identity.id,
        recipe_version: recipe.identity.version,
        engine: CompileCacheEngineIdentity {
            name: &recipe.engine.name,
            version: &recipe.engine.version,
            image_digest: &recipe.engine.image_digest,
            image_architecture: &recipe.engine.image_architecture,
            accelerator: recipe.engine.accelerator,
            entrypoint: &recipe.engine.entrypoint,
            argv: &recipe.engine.argv,
            substitutions: &recipe.engine.substitutions,
        },
        model: CompileCacheModelIdentity {
            repository: &recipe.model.repository,
            format: &recipe.model.format,
            precision: &recipe.model.precision,
            tokenizer_sha256: &recipe.model.tokenizer_sha256,
            parser: &recipe.model.parser,
            parser_sha256: &recipe.model.parser_sha256,
            remote_code: &recipe.model.remote_code,
            files: &recipe.model.files,
        },
        model_commit,
        context_ceiling: recipe.resources.context_ceiling,
        image_run_as_uid: recipe.evidence.image_run_as_uid,
        isolation_run_as_uid: recipe.isolation.run_as_uid,
    };
    let identity = serde_json::to_vec(&identity).map_err(|_| ())?;
    Ok(format!("sha256-{:x}", Sha256::digest(identity)))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    pub schema: String,
    pub socket: PathBuf,
    pub agent_uid: u32,
    pub recipes_dir: PathBuf,
    pub engine_policy: PathBuf,
    pub resources_policy: PathBuf,
    pub host: RecipeHost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutorAction {
    action: ExecutorActionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExecutorActionKind {
    Health,
    InspectProtectedHost,
    InspectDockerVersion,
    InspectResources,
    InspectEmergencyRecords,
    InspectRecipes(RecipeQuery),
    PrepareInstance(StartInstanceInput),
    StartInstance(StartInstanceInput),
    PromoteRestartPolicy(StopInstanceInput),
    DisableRestartPolicy(StopInstanceInput),
    StopInstance(StopInstanceInput),
    InspectInstance(StopInstanceInput),
    ReadManagedLogs(LogInput),
    ReconcileScan(Vec<ReconcileExpectation>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipeQuery {
    model_repository: Option<String>,
    model_commit: Option<String>,
    objective: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ExecutorResult {
    Health {
        health: ExecutorHealth,
    },
    InspectProtectedHost {
        host: ProtectedHostSnapshot,
    },
    InspectDockerVersion {
        docker: DockerCapability,
    },
    InspectResources {
        snapshot: HostResourceSnapshot,
        policy: ResourcePolicy,
    },
    InspectEmergencyRecords {
        records: Vec<super::resources::EmergencyRecord>,
    },
    InspectRecipes {
        catalog: RecipeCatalogDocument,
    },
    PrepareInstance {
        startup_deadline_seconds: u64,
    },
    StartInstance {
        observed: ObservedEngine,
    },
    PromoteRestartPolicy {
        observed: ObservedEngine,
    },
    DisableRestartPolicy {
        observed: ObservedEngine,
    },
    StopInstance,
    InspectInstance {
        observed: Option<ObservedEngine>,
    },
    ReadManagedLogs {
        logs: EngineLogs,
    },
    ReconcileScan {
        scan: ReconcileScan,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct SparkUidAuthorizer {
    agent_uid: u32,
}

impl SparkUidAuthorizer {
    pub const fn new(agent_uid: u32) -> Self {
        Self { agent_uid }
    }
}

impl PeerAuthorizer for SparkUidAuthorizer {
    fn authorize(&self, credentials: Option<PeerCredentials>) -> bool {
        credentials.is_some_and(|peer| peer.uid() == self.agent_uid)
    }
}

type DockerFuture = Pin<Box<dyn Future<Output = Result<DockerCapability, ()>> + Send>>;

trait DockerInspector: Send + Sync + 'static {
    fn inspect(&self, cancellation: CancellationToken) -> DockerFuture;
}

type RuntimeFuture<T> = Pin<Box<dyn Future<Output = Result<T, ()>> + Send>>;

trait ContainerRuntime: Send + Sync + 'static {
    fn ensure_network(&self, name: String) -> RuntimeFuture<String>;
    fn ensure_image(&self, image: String, architecture: String) -> RuntimeFuture<()>;
    fn start(&self, spec: ContainerSpec) -> RuntimeFuture<ObservedEngine>;
    fn promote_restart(&self, input: StopInstanceInput) -> RuntimeFuture<ObservedEngine>;
    fn disable_restart(&self, input: StopInstanceInput) -> RuntimeFuture<ObservedEngine>;
    fn inspect(&self, input: StopInstanceInput) -> RuntimeFuture<Option<ObservedEngine>>;
    fn stop(&self, input: StopInstanceInput) -> RuntimeFuture<()>;
    fn logs(&self, input: LogInput) -> RuntimeFuture<EngineLogs>;
    fn scan_managed(&self) -> RuntimeFuture<Vec<ManagedContainerObservation>>;
    fn quarantine(&self, container_id: String) -> RuntimeFuture<()>;
}

struct BollardContainerRuntime;

fn exact_image_reference(image: &str) -> bool {
    let Some((repository, digest)) = image.split_once("@sha256:") else {
        return false;
    };
    !repository.is_empty()
        && !repository.contains('@')
        && repository
            .rsplit('/')
            .next()
            .is_some_and(|name| !name.contains(':'))
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn exact_image_matches(image: &str, architecture: &str, inspected: &ImageInspect) -> bool {
    exact_image_reference(image)
        && inspected.architecture.as_deref() == Some(architecture)
        && inspected
            .repo_digests
            .as_ref()
            .is_some_and(|digests| digests.iter().any(|candidate| candidate == image))
}

trait ImageStore: Send + Sync {
    fn inspect_exact(&self, image: String) -> RuntimeFuture<ImageInspect>;
    fn pull_exact(&self, image: String) -> RuntimeFuture<()>;
}

struct BollardImageStore(Docker);

impl ImageStore for BollardImageStore {
    fn inspect_exact(&self, image: String) -> RuntimeFuture<ImageInspect> {
        let docker = self.0.clone();
        Box::pin(async move { docker.inspect_image(&image).await.map_err(|_| ()) })
    }

    fn pull_exact(&self, image: String) -> RuntimeFuture<()> {
        let docker = self.0.clone();
        Box::pin(async move {
            docker
                .create_image(
                    Some(
                        CreateImageOptionsBuilder::default()
                            .from_image(&image)
                            .build(),
                    ),
                    None,
                    None,
                )
                .try_collect::<Vec<_>>()
                .await
                .map(|_| ())
                .map_err(|_| ())
        })
    }
}

async fn ensure_exact_image(
    store: &dyn ImageStore,
    image: &str,
    architecture: &str,
) -> Result<(), ()> {
    if !exact_image_reference(image) {
        return Err(());
    }
    if store
        .inspect_exact(image.into())
        .await
        .is_ok_and(|inspected| exact_image_matches(image, architecture, &inspected))
    {
        return Ok(());
    }
    let _pull_result = store.pull_exact(image.into()).await;
    match store.inspect_exact(image.into()).await {
        Ok(inspected) if exact_image_matches(image, architecture, &inspected) => Ok(()),
        _ => Err(()),
    }
}

fn docker_failure_diagnostic(stage: &str, error: &bollard::errors::Error) -> String {
    let stage = match stage {
        "create" | "start" | "inspect" => stage,
        _ => "unknown",
    };
    let (cause, status) = match error {
        bollard::errors::Error::DockerResponseServerError {
            status_code,
            message,
        } => {
            let message = message.to_ascii_lowercase();
            let cause = if message.contains("unknown or invalid runtime") {
                "nvidia-runtime-unavailable"
            } else if message.contains("could not select device driver") {
                "nvidia-device-request-unavailable"
            } else if message.contains("network") {
                "managed-network-rejected"
            } else if message.contains("memory") {
                "memory-limit-rejected"
            } else if message.contains("permission denied") {
                "docker-permission-denied"
            } else {
                "docker-api-rejected"
            };
            (cause, Some(*status_code))
        }
        bollard::errors::Error::RequestTimeoutError => ("docker-api-timeout", None),
        _ => ("docker-transport-failed", None),
    };
    match status {
        Some(status) => format!("stage={stage} cause={cause} status={status}"),
        None => format!("stage={stage} cause={cause}"),
    }
}

fn log_docker_failure(stage: &str, error: &bollard::errors::Error) {
    tracing::error!(
        target: "sy::spark::executor",
        diagnostic = docker_failure_diagnostic(stage, error),
        "managed container operation failed"
    );
}

impl BollardContainerRuntime {
    async fn docker() -> Result<Docker, ()> {
        let docker = Docker::connect_with_socket(
            "/var/run/docker.sock",
            DOCKER_TIMEOUT_SECONDS,
            API_DEFAULT_VERSION,
        )
        .map_err(|_| ())?;
        docker.negotiate_version().await.map_err(|_| ())
    }
}

impl ContainerRuntime for BollardContainerRuntime {
    fn ensure_network(&self, name: String) -> RuntimeFuture<String> {
        Box::pin(async move {
            let docker = Self::docker().await?;
            if let Ok(network) = docker.inspect_network(&name, None).await {
                let labels = network.labels.unwrap_or_default();
                if network.internal != Some(true)
                    || network.driver.as_deref() != Some("bridge")
                    || labels.get("io.sy.spark.managed").map(String::as_str) != Some("true")
                {
                    return Err(());
                }
                return network.id.ok_or(());
            }
            let labels = std::collections::HashMap::from([
                ("io.sy.spark.managed".into(), "true".into()),
                ("io.sy.spark.role".into(), "network".into()),
            ]);
            docker
                .create_network(NetworkCreateRequest {
                    name,
                    driver: Some("bridge".into()),
                    internal: Some(true),
                    attachable: Some(false),
                    ingress: Some(false),
                    labels: Some(labels),
                    ..Default::default()
                })
                .await
                .map(|created| created.id)
                .map_err(|_| ())
        })
    }

    fn ensure_image(&self, image: String, architecture: String) -> RuntimeFuture<()> {
        Box::pin(async move {
            let docker = Self::docker().await?;
            ensure_exact_image(&BollardImageStore(docker), &image, &architecture).await
        })
    }

    fn start(&self, spec: ContainerSpec) -> RuntimeFuture<ObservedEngine> {
        Box::pin(async move {
            let supplemental_groups = match validate_mount_sources(&spec) {
                Ok(groups) => groups,
                Err(error) => {
                    tracing::error!(
                        target: "sy::spark::executor",
                        stage = "mount-validation",
                        cause = error.code(),
                        "managed container preparation failed"
                    );
                    return Err(());
                }
            };
            let tmpfs_gid = supplemental_groups.first().ok_or(())?.clone();
            let docker = Self::docker().await?;
            let labels = spec.labels.clone().into_iter().collect();
            let mounts = spec
                .mounts
                .iter()
                .map(|mount| Mount {
                    target: Some(mount.target.clone()),
                    source: Some(mount.source.clone()),
                    typ: Some(MountType::BIND),
                    read_only: Some(mount.read_only),
                    ..Default::default()
                })
                .collect();
            let tmpfs = spec
                .tmpfs
                .iter()
                .map(|path| (path.clone(), tmpfs_options(spec.run_as_uid, &tmpfs_gid)))
                .collect();
            let networking_config = NetworkingConfig {
                endpoints_config: Some(std::collections::HashMap::from([(
                    spec.network.clone(),
                    EndpointSettings::default(),
                )])),
            };
            let body = ContainerCreateBody {
                user: Some(spec.run_as_uid.to_string()),
                attach_stdin: Some(false),
                attach_stdout: Some(false),
                attach_stderr: Some(false),
                tty: Some(false),
                open_stdin: Some(false),
                env: Some(spec.environment.clone()),
                cmd: Some(spec.argv.clone()),
                image: Some(spec.image.clone()),
                entrypoint: Some(spec.entrypoint.clone()),
                labels: Some(labels),
                host_config: Some(HostConfig {
                    memory: Some(i64::try_from(spec.memory_bytes).map_err(|_| ())?),
                    memory_swap: Some(i64::try_from(spec.memory_bytes).map_err(|_| ())?),
                    pids_limit: Some(spec.pid_limit.into()),
                    init: Some(true),
                    network_mode: Some(spec.network.clone()),
                    port_bindings: None,
                    restart_policy: Some(restart_policy(RestartPolicyNameEnum::NO)),
                    mounts: Some(mounts),
                    cap_drop: Some(spec.cap_drop.clone()),
                    readonly_rootfs: Some(spec.read_only_rootfs),
                    security_opt: Some(security_options(&spec)),
                    tmpfs: Some(tmpfs),
                    group_add: Some(supplemental_groups),
                    device_requests: match spec.accelerator {
                        Accelerator::Cpu => None,
                        Accelerator::Nvidia => Some(vec![DeviceRequest {
                            driver: Some("nvidia".into()),
                            count: Some(-1),
                            capabilities: Some(vec![vec!["gpu".into()]]),
                            ..Default::default()
                        }]),
                    },
                    ..Default::default()
                }),
                networking_config: Some(networking_config),
                ..Default::default()
            };
            let created = match docker
                .create_container(
                    Some(
                        CreateContainerOptionsBuilder::default()
                            .name(&spec.name)
                            .build(),
                    ),
                    body,
                )
                .await
            {
                Ok(created) => created,
                Err(error) => {
                    log_docker_failure("create", &error);
                    return Err(());
                }
            };
            if let Err(error) = docker.start_container(&created.id, None).await {
                log_docker_failure("start", &error);
                let _ = docker
                    .remove_container(
                        &created.id,
                        Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                    )
                    .await;
                return Err(());
            }
            match inspect_exact(&docker, &created.id, &spec).await {
                Ok(observed) => Ok(observed),
                Err(()) => {
                    tracing::error!(
                        target: "sy::spark::executor",
                        stage = "inspect",
                        cause = "identity-or-running-state-mismatch",
                        "managed container operation failed"
                    );
                    let _ = docker
                        .remove_container(
                            &created.id,
                            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                        )
                        .await;
                    Err(())
                }
            }
        })
    }

    fn promote_restart(&self, input: StopInstanceInput) -> RuntimeFuture<ObservedEngine> {
        Box::pin(async move {
            let docker = Self::docker().await?;
            let observed = inspect_input(&docker, &input).await?.ok_or(())?;
            docker
                .update_container(
                    &observed.container_id,
                    ContainerUpdateBody {
                        restart_policy: Some(restart_policy(RestartPolicyNameEnum::UNLESS_STOPPED)),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|_| ())?;
            Ok(ObservedEngine {
                restart_policy: "unless-stopped".into(),
                ..observed
            })
        })
    }

    fn disable_restart(&self, input: StopInstanceInput) -> RuntimeFuture<ObservedEngine> {
        Box::pin(async move {
            let docker = Self::docker().await?;
            let observed = inspect_input(&docker, &input).await?.ok_or(())?;
            docker
                .update_container(
                    &observed.container_id,
                    ContainerUpdateBody {
                        restart_policy: Some(restart_policy(RestartPolicyNameEnum::NO)),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|_| ())?;
            Ok(ObservedEngine {
                restart_policy: "no".into(),
                ..observed
            })
        })
    }

    fn inspect(&self, input: StopInstanceInput) -> RuntimeFuture<Option<ObservedEngine>> {
        Box::pin(async move {
            let docker = Self::docker().await?;
            inspect_input(&docker, &input).await
        })
    }

    fn stop(&self, input: StopInstanceInput) -> RuntimeFuture<()> {
        Box::pin(async move {
            if input.grace_seconds > 300 {
                return Err(());
            }
            let docker = Self::docker().await?;
            let Some((container_id, running)) = inspect_stop_target(&docker, &input).await? else {
                return Ok(());
            };
            docker
                .update_container(
                    &container_id,
                    ContainerUpdateBody {
                        restart_policy: Some(restart_policy(RestartPolicyNameEnum::NO)),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|_| ())?;
            if running {
                docker
                    .stop_container(
                        &container_id,
                        Some(
                            StopContainerOptionsBuilder::default()
                                .t(input.grace_seconds as i32)
                                .build(),
                        ),
                    )
                    .await
                    .map_err(|_| ())?;
            }
            docker
                .remove_container(
                    &container_id,
                    Some(
                        RemoveContainerOptionsBuilder::default()
                            .force(false)
                            .build(),
                    ),
                )
                .await
                .map_err(|_| ())
        })
    }

    fn logs(&self, input: LogInput) -> RuntimeFuture<EngineLogs> {
        Box::pin(async move {
            if input.limit == 0 || input.limit > MAX_ENGINE_LOG_LINES {
                return Err(());
            }
            let docker = Self::docker().await?;
            let lookup = StopInstanceInput {
                instance_id: input.instance_id,
                generation: input.generation,
                grace_seconds: 0,
            };
            let observed = inspect_input(&docker, &lookup).await?.ok_or(())?;
            let output = docker
                .logs(
                    &observed.container_id,
                    Some(
                        LogsOptionsBuilder::default()
                            .stdout(true)
                            .stderr(true)
                            .timestamps(true)
                            .since(i32::try_from(input.cursor / 1_000_000_000).unwrap_or(i32::MAX))
                            .tail(&input.limit.saturating_add(1).to_string())
                            .build(),
                    ),
                )
                .try_collect::<Vec<_>>()
                .await
                .map_err(|_| ())?;
            let mut lines = output
                .into_iter()
                .flat_map(|chunk| {
                    chunk
                        .to_string()
                        .lines()
                        .filter_map(timestamped_log_line)
                        .collect::<Vec<_>>()
                })
                .filter(|(cursor, _)| *cursor > input.cursor)
                .collect::<Vec<_>>();
            let truncated = lines.len() > input.limit;
            if truncated {
                lines.drain(..lines.len() - input.limit);
            }
            let next_cursor = lines
                .last()
                .map(|(cursor, _)| *cursor)
                .unwrap_or(input.cursor);
            Ok(EngineLogs {
                cursor: input.cursor,
                next_cursor,
                truncated,
                lines: lines.into_iter().map(|(_, line)| line).collect(),
            })
        })
    }

    fn scan_managed(&self) -> RuntimeFuture<Vec<ManagedContainerObservation>> {
        Box::pin(async move {
            let docker = Self::docker().await?;
            let filters: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::from([(
                    "label".into(),
                    vec!["io.sy.spark.managed=true".into()],
                )]);
            let listed = docker
                .list_containers(Some(
                    ListContainersOptionsBuilder::default()
                        .all(true)
                        .filters(&filters)
                        .build(),
                ))
                .await
                .map_err(|_| ())?;
            if listed.len() > MAX_MANAGED_CONTAINERS {
                return Err(());
            }
            let mut observed = Vec::with_capacity(listed.len());
            for summary in listed {
                let id = summary.id.ok_or(())?;
                let inspect = docker.inspect_container(&id, None).await.map_err(|_| ())?;
                observed.push(managed_observation(inspect)?);
            }
            Ok(observed)
        })
    }

    fn quarantine(&self, container_id: String) -> RuntimeFuture<()> {
        Box::pin(async move {
            if container_id.len() != 64
                || !container_id.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(());
            }
            let docker = Self::docker().await?;
            let inspect = docker
                .inspect_container(&container_id, None)
                .await
                .map_err(|_| ())?;
            let managed = inspect
                .config
                .as_ref()
                .and_then(|config| config.labels.as_ref())
                .and_then(|labels| labels.get("io.sy.spark.managed"))
                .map(String::as_str);
            if managed != Some("true") {
                return Err(());
            }
            docker
                .update_container(
                    &container_id,
                    ContainerUpdateBody {
                        restart_policy: Some(restart_policy(RestartPolicyNameEnum::NO)),
                        ..Default::default()
                    },
                )
                .await
                .map(|_| ())
                .map_err(|_| ())
        })
    }
}

fn managed_observation(
    inspect: bollard::models::ContainerInspectResponse,
) -> Result<ManagedContainerObservation, ()> {
    let labels = inspect
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or(())?;
    let networks = inspect
        .network_settings
        .as_ref()
        .and_then(|settings| settings.networks.as_ref())
        .map(|values| values.keys().cloned().collect())
        .unwrap_or_default();
    let restart_policy = inspect
        .host_config
        .as_ref()
        .and_then(|host| host.restart_policy.as_ref())
        .and_then(|policy| policy.name)
        .map(|name| name.to_string())
        .unwrap_or_else(|| "no".into());
    Ok(ManagedContainerObservation {
        container_id: inspect.id.ok_or(())?,
        name: inspect
            .name
            .unwrap_or_default()
            .trim_start_matches('/')
            .into(),
        instance_id: labels.get("io.sy.spark.instance").cloned(),
        generation: labels
            .get("io.sy.spark.generation")
            .and_then(|value| value.parse().ok()),
        role: labels.get("io.sy.spark.role").cloned(),
        model_commit: labels.get("io.sy.spark.model_commit").cloned(),
        model_repository: labels.get("io.sy.spark.model_repository").cloned(),
        recipe_id: labels.get("io.sy.spark.recipe").cloned(),
        image: inspect.config.and_then(|config| config.image),
        networks,
        restart_policy,
    })
}

fn restart_policy(name: RestartPolicyNameEnum) -> RestartPolicy {
    RestartPolicy {
        name: Some(name),
        maximum_retry_count: Some(0),
    }
}

fn security_options(spec: &ContainerSpec) -> Vec<String> {
    let mut options = vec!["no-new-privileges=true".into()];
    if spec.seccomp != "default" {
        options.push(format!("seccomp={}", spec.seccomp));
    }
    options
}

fn tmpfs_options(run_as_uid: u32, supplemental_gid: &str) -> String {
    format!(
        "rw,noexec,nosuid,nodev,size=1073741824,mode=1770,uid={run_as_uid},gid={supplemental_gid}"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountValidationError {
    InvalidPath,
    InvalidOwnership,
    InvalidMode,
    PermissionDenied,
    Unavailable,
}

impl MountValidationError {
    const fn code(self) -> &'static str {
        match self {
            Self::InvalidPath => "mount-path-invalid",
            Self::InvalidOwnership => "mount-ownership-invalid",
            Self::InvalidMode => "mount-mode-invalid",
            Self::PermissionDenied => "mount-permission-denied",
            Self::Unavailable => "mount-metadata-unavailable",
        }
    }
}

fn mount_io(error: std::io::Error) -> MountValidationError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        MountValidationError::PermissionDenied
    } else {
        MountValidationError::Unavailable
    }
}

fn validate_mount_sources(spec: &ContainerSpec) -> Result<Vec<String>, MountValidationError> {
    validate_mount_sources_at(
        spec,
        Path::new(&spec.model_cache_root),
        Path::new(&spec.compile_cache_root),
        0,
    )
}

fn validate_mount_sources_at(
    spec: &ContainerSpec,
    model_root: &Path,
    cache_root: &Path,
    cache_owner_uid: u32,
) -> Result<Vec<String>, MountValidationError> {
    let cache_root_metadata = std::fs::symlink_metadata(cache_root).map_err(mount_io)?;
    if !cache_root_metadata.is_dir() || cache_root_metadata.uid() != cache_owner_uid {
        return Err(MountValidationError::InvalidOwnership);
    }
    if cache_root_metadata.permissions().mode() & 0o777 != 0o750 {
        return Err(MountValidationError::InvalidMode);
    }
    let cache_gid = cache_root_metadata.gid();
    for mount in &spec.mounts {
        let source = Path::new(&mount.source);
        let expected = if mount.read_only {
            model_root
        } else {
            cache_root
        };
        if !source.starts_with(expected) || !mount.target.starts_with('/') {
            return Err(MountValidationError::InvalidPath);
        }
        if mount.read_only {
            let canonical = source.canonicalize().map_err(mount_io)?;
            if !canonical.starts_with(expected) {
                return Err(MountValidationError::InvalidPath);
            }
            let metadata = canonical.metadata().map_err(mount_io)?;
            let mode = metadata.permissions().mode() & 0o777;
            if !metadata.is_dir() || metadata.gid() != cache_gid {
                return Err(MountValidationError::InvalidOwnership);
            }
            if mode & 0o022 != 0 || mode & 0o050 != 0o050 {
                return Err(MountValidationError::InvalidMode);
            }
        } else {
            if !source.exists() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o770)
                    .create(source)
                    .map_err(mount_io)?;
                std::fs::set_permissions(source, std::fs::Permissions::from_mode(0o770))
                    .map_err(mount_io)?;
            }
            let canonical = source.canonicalize().map_err(mount_io)?;
            if !canonical.starts_with(expected) {
                return Err(MountValidationError::InvalidPath);
            }
            let metadata = canonical.metadata().map_err(mount_io)?;
            if !metadata.is_dir()
                || metadata.uid() != cache_owner_uid
                || metadata.gid() != cache_gid
            {
                return Err(MountValidationError::InvalidOwnership);
            }
            if metadata.permissions().mode() & 0o777 != 0o770 {
                return Err(MountValidationError::InvalidMode);
            }
        }
    }
    Ok(vec![cache_gid.to_string()])
}

async fn inspect_input(
    docker: &Docker,
    input: &StopInstanceInput,
) -> Result<Option<ObservedEngine>, ()> {
    if input.instance_id.len() != 34 || input.generation == 0 {
        return Err(());
    }
    let name = format!("sy-spark-{}-g{}", input.instance_id, input.generation);
    let inspect = match docker.inspect_container(&name, None).await {
        Ok(inspect) => inspect,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => return Ok(None),
        Err(_) => return Err(()),
    };
    let labels = inspect
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or(())?;
    if !exact_managed_identity(labels, input) {
        return Err(());
    }
    observed_from_inspect(inspect, 0).map(Some)
}

async fn inspect_stop_target(
    docker: &Docker,
    input: &StopInstanceInput,
) -> Result<Option<(String, bool)>, ()> {
    if input.instance_id.len() != 34 || input.generation == 0 {
        return Err(());
    }
    let name = format!("sy-spark-{}-g{}", input.instance_id, input.generation);
    let inspect = match docker.inspect_container(&name, None).await {
        Ok(inspect) => inspect,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => return Ok(None),
        Err(_) => return Err(()),
    };
    let labels = inspect
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or(())?;
    if !exact_managed_identity(labels, input) {
        return Err(());
    }
    let running = inspect
        .state
        .as_ref()
        .and_then(|state| state.running)
        .unwrap_or(false);
    Ok(Some((inspect.id.ok_or(())?, running)))
}

fn exact_managed_identity(
    labels: &std::collections::HashMap<String, String>,
    input: &StopInstanceInput,
) -> bool {
    labels.get("io.sy.spark.managed").map(String::as_str) == Some("true")
        && labels.get("io.sy.spark.role").map(String::as_str) == Some("engine")
        && labels.get("io.sy.spark.instance").map(String::as_str)
            == Some(input.instance_id.as_str())
        && labels.get("io.sy.spark.generation").map(String::as_str)
            == Some(input.generation.to_string().as_str())
}

async fn inspect_exact(
    docker: &Docker,
    container_id: &str,
    spec: &ContainerSpec,
) -> Result<ObservedEngine, ()> {
    let inspect = docker
        .inspect_container(container_id, None)
        .await
        .map_err(|_| ())?;
    let labels = inspect
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or(())?;
    if spec
        .labels
        .iter()
        .any(|(key, value)| labels.get(key) != Some(value))
    {
        return Err(());
    }
    let mut observed = observed_from_inspect(inspect, spec.port)?;
    observed.health_method = spec.health_method.clone();
    observed.health_path = spec.health_path.clone();
    observed.allowed_routes = spec.allowed_routes.clone();
    observed.gateway_profile = spec.gateway_profile.clone();
    observed.served_model = spec.served_model.clone();
    observed.semantic_prompt = spec.semantic_prompt.clone();
    observed.semantic_max_tokens = spec.semantic_max_tokens;
    observed.startup_deadline_seconds = spec.startup_deadline_seconds;
    Ok(observed)
}

fn observed_from_inspect(
    inspect: bollard::models::ContainerInspectResponse,
    expected_port: u16,
) -> Result<ObservedEngine, ()> {
    let labels = inspect
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or(())?;
    let instance_id = labels.get("io.sy.spark.instance").cloned().ok_or(())?;
    let generation = labels
        .get("io.sy.spark.generation")
        .and_then(|value| value.parse().ok())
        .ok_or(())?;
    let networks = inspect
        .network_settings
        .as_ref()
        .and_then(|network| network.networks.as_ref())
        .ok_or(())?;
    if networks.len() != 1 {
        return Err(());
    }
    let endpoint = networks.values().next().ok_or(())?;
    let address = endpoint
        .ip_address
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or(())?;
    let network_id = endpoint
        .network_id
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or(())?;
    let running = inspect
        .state
        .as_ref()
        .and_then(|state| state.running)
        .unwrap_or(false);
    let restart_policy = inspect
        .host_config
        .as_ref()
        .and_then(|host| host.restart_policy.as_ref())
        .and_then(|policy| policy.name)
        .map(|name| name.to_string())
        .unwrap_or_else(|| "no".into());
    let init_pid = inspect
        .state
        .as_ref()
        .and_then(|state| state.pid)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
        .ok_or(())?;
    let (pid_start_time_ticks, cgroup_path) = process_identity(init_pid)?;
    if inspect
        .network_settings
        .as_ref()
        .and_then(|network| network.ports.as_ref())
        .is_some_and(has_published_ports)
    {
        return Err(());
    }
    Ok(ObservedEngine {
        instance_id,
        generation,
        container_id: inspect.id.ok_or(())?,
        network_id,
        address,
        port: expected_port,
        running,
        restart_policy,
        health_method: "GET".into(),
        health_path: "/health".into(),
        allowed_routes: vec![("GET".into(), "/health".into())],
        gateway_profile: GatewayProfile::text(),
        served_model: String::new(),
        semantic_prompt: String::new(),
        semantic_max_tokens: 0,
        startup_deadline_seconds: 0,
        init_pid,
        pid_start_time_ticks,
        cgroup_path,
    })
}

fn has_published_ports(
    ports: &std::collections::HashMap<String, Option<Vec<bollard::models::PortBinding>>>,
) -> bool {
    ports
        .values()
        .any(|bindings| bindings.as_ref().is_some_and(|values| !values.is_empty()))
}

fn process_identity(pid: u32) -> Result<(u64, String), ()> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|_| ())?;
    let after_name = stat.rsplit_once(") ").map(|(_, rest)| rest).ok_or(())?;
    let start_time_ticks = after_name
        .split_ascii_whitespace()
        .nth(19)
        .and_then(|value| value.parse().ok())
        .ok_or(())?;
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).map_err(|_| ())?;
    let path = cgroup
        .lines()
        .find_map(|line| line.split_once("::").map(|(_, path)| path))
        .and_then(|path| path.strip_prefix('/'))
        .filter(|path| !path.is_empty())
        .ok_or(())?;
    Ok((start_time_ticks, path.into()))
}

fn redact_log_line(line: &str) -> String {
    let lowercase_line = line.to_ascii_lowercase();
    if lowercase_line.contains("authorization")
        || lowercase_line.contains("bearer ")
        || lowercase_line.contains("token=")
        || line.contains("hf_")
    {
        return "[REDACTED]".into();
    }
    line.split_whitespace()
        .map(|part| {
            let lowercase = part.to_ascii_lowercase();
            if part.starts_with("hf_")
                || lowercase.starts_with("bearer")
                || lowercase.contains("token=")
                || lowercase.contains("authorization=")
            {
                "[REDACTED]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn timestamped_log_line(line: &str) -> Option<(u64, String)> {
    let timestamp = line.split_ascii_whitespace().next()?;
    let timestamp = chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()?
        .timestamp_nanos_opt()?;
    Some((u64::try_from(timestamp).ok()?, redact_log_line(line)))
}

fn cgroup_memory_current(root: &Path, relative: &str) -> Result<u64, ()> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(());
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(root.join(relative).join("memory.current"))
        .map_err(|_| ())?;
    let mut value = String::new();
    file.read_to_string(&mut value).map_err(|_| ())?;
    value.trim().parse().map_err(|_| ())
}

struct BollardDockerInspector;

impl DockerInspector for BollardDockerInspector {
    fn inspect(&self, cancellation: CancellationToken) -> DockerFuture {
        Box::pin(async move {
            let docker = Docker::connect_with_socket(
                "/var/run/docker.sock",
                DOCKER_TIMEOUT_SECONDS,
                API_DEFAULT_VERSION,
            )
            .map_err(|_| ())?;
            let docker = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(()),
                result = docker.negotiate_version() => result.map_err(|_| ())?,
            };
            let version = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(()),
                result = docker.version() => result.map_err(|_| ())?,
            };
            Ok(DockerCapability {
                schema: "sy.spark.executor.docker/v1".into(),
                transport: "unix".into(),
                version: version.version.ok_or(())?,
                api_version: version.api_version.ok_or(())?,
                minimum_api_version: version.min_api_version.ok_or(())?,
                os: version.os.ok_or(())?,
                architecture: version.arch.ok_or(())?,
                experimental: version.experimental.unwrap_or(false),
            })
        })
    }
}

#[derive(Default)]
struct Heartbeats {
    guard: AtomicU64,
    events: AtomicU64,
    event_epoch: AtomicU64,
}

impl Heartbeats {
    fn mark_guard(&self) {
        self.guard.store(now_seconds(), Ordering::Release);
    }

    fn mark_events(&self) {
        self.events.store(now_seconds(), Ordering::Release);
    }

    fn wake_for_managed_event(&self) {
        let _ = self
            .event_epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                Some(epoch.saturating_add(1))
            });
    }

    fn event_epoch(&self) -> u64 {
        self.event_epoch.load(Ordering::Acquire)
    }

    fn healthy(&self) -> (bool, bool) {
        let now = now_seconds();
        let recent = |value| now.saturating_sub(value) <= HEARTBEAT_STALE_SECONDS;
        (
            recent(self.guard.load(Ordering::Acquire)),
            recent(self.events.load(Ordering::Acquire)),
        )
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct ResourceMonitor {
    sampler: Mutex<Box<dyn HostSampler>>,
    latest: RwLock<Option<HostResourceSnapshot>>,
    policy: ResourcePolicy,
    heartbeats: Arc<Heartbeats>,
    guard: Mutex<super::resources::EmergencyGuard>,
    managed: RwLock<Vec<super::resources::ManagedEngine>>,
    identities: RwLock<BTreeMap<(String, u64), super::resources::ManagedCgroupIdentity>>,
    pending_emergency: Mutex<Option<super::resources::EmergencyDecision>>,
    emergency_journal: PathBuf,
    _cgroup_kill: fn(
        &Path,
        &super::resources::ManagedCgroupIdentity,
        &super::resources::ManagedCgroupIdentity,
    ) -> Result<(), super::resources::CgroupKillError>,
}

impl ResourceMonitor {
    fn new(
        sampler: Box<dyn HostSampler>,
        policy: ResourcePolicy,
        heartbeats: Arc<Heartbeats>,
        emergency_journal: PathBuf,
    ) -> Self {
        Self {
            sampler: Mutex::new(sampler),
            latest: RwLock::new(None),
            guard: Mutex::new(super::resources::EmergencyGuard::new(policy.clone())),
            managed: RwLock::new(Vec::new()),
            identities: RwLock::new(BTreeMap::new()),
            pending_emergency: Mutex::new(None),
            emergency_journal,
            _cgroup_kill: super::resources::kill_managed_cgroup,
            policy,
            heartbeats,
        }
    }

    fn sample_once(&self) -> bool {
        self.refresh_managed_memory_at(Path::new("/sys/fs/cgroup"));
        let snapshot = self
            .sampler
            .lock()
            .ok()
            .and_then(|mut sampler| sampler.sample().ok());
        let Some(snapshot) = snapshot.filter(HostResourceSnapshot::is_complete) else {
            return false;
        };
        let Ok(mut latest) = self.latest.write() else {
            return false;
        };
        *latest = Some(snapshot);
        let decision = self
            .guard
            .lock()
            .ok()
            .and_then(|mut guard| guard.observe(latest.as_ref()?, &self.managed.read().ok()?));
        if let Some(decision) = decision {
            let record = super::resources::EmergencyRecord::from_decision(decision.clone());
            if super::resources::append_emergency_record(&self.emergency_journal, &record).is_err()
            {
                return false;
            }
            if let Ok(mut pending) = self.pending_emergency.lock() {
                *pending = Some(decision);
            } else {
                return false;
            }
        }
        self.heartbeats.mark_guard();
        true
    }

    fn snapshot(&self) -> Option<(HostResourceSnapshot, ResourcePolicy)> {
        let snapshot = self.latest.read().ok()?.clone()?;
        (super::resources::unix_millis().saturating_sub(snapshot.observed_at_unix_ms)
            <= self.policy.max_snapshot_age_ms())
        .then(|| (snapshot, self.policy.clone()))
    }

    fn emergency_records(&self) -> Result<Vec<super::resources::EmergencyRecord>, ()> {
        super::resources::read_emergency_records(&self.emergency_journal).map_err(|_| ())
    }

    fn sampling_interval_ms(&self) -> u64 {
        self.managed
            .read()
            .map(|engines| super::resources::guard_interval_ms(&self.policy, &engines))
            .unwrap_or(self.policy.startup_guard_interval_ms)
    }

    fn observe_engine(&self, observed: &ObservedEngine, phase: super::resources::EnginePhase) {
        let engine = super::resources::ManagedEngine {
            instance_id: observed.instance_id.clone(),
            generation: observed.generation,
            phase,
            started_sequence: observed.generation,
            memory_bytes: 0,
            previous_memory_bytes: 0,
        };
        if let Ok(mut managed) = self.managed.write() {
            managed.retain(|current| current.instance_id != observed.instance_id);
            managed.push(engine);
        }
        let identity = super::resources::ManagedCgroupIdentity {
            managed_label: true,
            engine_role: "engine".into(),
            instance_id: observed.instance_id.clone(),
            generation: observed.generation,
            container_id: observed.container_id.clone(),
            init_pid: observed.init_pid,
            pid_start_time_ticks: observed.pid_start_time_ticks,
            cgroup_path: observed.cgroup_path.clone(),
        };
        if let Ok(mut identities) = self.identities.write() {
            identities.insert(
                (observed.instance_id.clone(), observed.generation),
                identity,
            );
        }
        self.refresh_managed_memory_at(Path::new("/sys/fs/cgroup"));
    }

    fn refresh_managed_memory_at(&self, cgroup_root: &Path) {
        let Ok(identities) = self.identities.read() else {
            return;
        };
        let Ok(mut managed) = self.managed.write() else {
            return;
        };
        for engine in managed.iter_mut() {
            let Some(identity) = identities.get(&(engine.instance_id.clone(), engine.generation))
            else {
                continue;
            };
            let Ok(current) = cgroup_memory_current(cgroup_root, &identity.cgroup_path) else {
                continue;
            };
            engine.previous_memory_bytes = engine.memory_bytes;
            engine.memory_bytes = current;
        }
    }

    fn forget_engine(&self, instance_id: &str, generation: u64) {
        if let Ok(mut managed) = self.managed.write() {
            managed.retain(|engine| {
                engine.instance_id != instance_id || engine.generation != generation
            });
        }
        if let Ok(mut identities) = self.identities.write() {
            identities.remove(&(instance_id.into(), generation));
        }
    }

    fn take_emergency(&self) -> Option<super::resources::EmergencyDecision> {
        self.pending_emergency.lock().ok()?.take()
    }

    fn identity(
        &self,
        decision: &super::resources::EmergencyDecision,
    ) -> Option<super::resources::ManagedCgroupIdentity> {
        self.identities
            .read()
            .ok()?
            .get(&(decision.instance_id.clone(), decision.generation))
            .cloned()
    }

    fn suppress(&self, decision: &super::resources::EmergencyDecision) -> bool {
        self.guard
            .lock()
            .map(|mut guard| guard.suppress(decision))
            .is_ok()
    }
}

#[derive(Deserialize)]
struct ResourcePolicyFile {
    resources: ResourcePolicyConfig,
}

fn load_resource_policy(path: &Path) -> Result<ResourcePolicy, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("read Spark resource policy: {error}"))?;
    let file: ResourcePolicyFile =
        toml::from_str(&text).map_err(|error| format!("parse Spark resource policy: {error}"))?;
    file.resources.policy().map_err(str::to_owned)
}

struct ExecutorHandler {
    agent_uid: u32,
    cancellation: Arc<CancelRegistry>,
    docker: Arc<dyn DockerInspector>,
    runtime: Arc<dyn ContainerRuntime>,
    heartbeats: Arc<Heartbeats>,
    hostname_path: PathBuf,
    kernel_release: String,
    machine_id_path: PathBuf,
    catalog: Arc<RecipeCatalog>,
    engine_policy: Arc<EnginePolicy>,
    recipe_host: RecipeHost,
    resources: Option<Arc<ResourceMonitor>>,
}

impl ExecutorHandler {
    fn production(
        config: &ExecutorConfig,
        heartbeats: Arc<Heartbeats>,
        resources: Arc<ResourceMonitor>,
        runtime: Arc<dyn ContainerRuntime>,
    ) -> Result<Self, String> {
        let catalog = RecipeCatalog::load_legacy(&config.recipes_dir)?;
        let engine_policy = EnginePolicy::load(&config.engine_policy)?;
        Ok(Self {
            agent_uid: config.agent_uid,
            cancellation: Arc::new(CancelRegistry::new()),
            docker: Arc::new(BollardDockerInspector),
            runtime,
            heartbeats,
            hostname_path: "/etc/hostname".into(),
            kernel_release: rustix::system::uname()
                .release()
                .to_string_lossy()
                .into_owned(),
            machine_id_path: "/etc/machine-id".into(),
            catalog: Arc::new(catalog),
            engine_policy: Arc::new(engine_policy),
            recipe_host: config.host.clone(),
            resources: Some(resources),
        })
    }

    async fn execute(
        &self,
        action: ExecutorAction,
        cancellation: CancellationToken,
    ) -> Result<ExecutorResult, ErrorCode> {
        match action.action {
            ExecutorActionKind::Health => {
                let (guard_heartbeat, event_heartbeat) = self.heartbeats.healthy();
                Ok(ExecutorResult::Health {
                    health: ExecutorHealth {
                        schema: "sy.spark.executor.health/v1".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                        authorized_agent_uid: self.agent_uid,
                        guard_heartbeat,
                        event_heartbeat,
                        event_epoch: self.heartbeats.event_epoch(),
                    },
                })
            }
            ExecutorActionKind::InspectProtectedHost => self
                .inspect_host()
                .map(|host| ExecutorResult::InspectProtectedHost { host })
                .map_err(|()| ErrorCode::Internal),
            ExecutorActionKind::InspectDockerVersion => self
                .docker
                .inspect(cancellation)
                .await
                .map(|docker| ExecutorResult::InspectDockerVersion { docker })
                .map_err(|()| ErrorCode::NotReady),
            ExecutorActionKind::InspectResources => self
                .resources
                .as_ref()
                .and_then(|resources| resources.snapshot())
                .map(|(snapshot, policy)| ExecutorResult::InspectResources { snapshot, policy })
                .ok_or(ErrorCode::NotReady),
            ExecutorActionKind::InspectEmergencyRecords => self
                .resources
                .as_ref()
                .and_then(|resources| resources.emergency_records().ok())
                .map(|records| ExecutorResult::InspectEmergencyRecords { records })
                .ok_or(ErrorCode::NotReady),
            ExecutorActionKind::InspectRecipes(query) => Ok(ExecutorResult::InspectRecipes {
                catalog: self.catalog.query(
                    &self.recipe_host,
                    query.model_repository.as_deref(),
                    query.model_commit.as_deref(),
                    &query.objective,
                    chrono::Utc::now(),
                ),
            }),
            ExecutorActionKind::PrepareInstance(input) => {
                if input.recipe_id != self.engine_policy.config().id {
                    return Err(ErrorCode::BadRequest);
                }
                let spec = build_generic_container_spec(&self.engine_policy, &input)
                    .map_err(|()| ErrorCode::BadRequest)?;
                let architecture = self.engine_policy.config().image_architecture.clone();
                let startup_deadline_seconds = self.engine_policy.config().startup_deadline_seconds;
                self.runtime
                    .ensure_network(self.engine_policy.config().network.clone())
                    .await
                    .map(|_| ())
                    .map_err(|()| ErrorCode::NotReady)?;
                self.runtime
                    .ensure_image(spec.image.clone(), architecture)
                    .await
                    .map_err(|()| ErrorCode::NotReady)?;
                Ok(ExecutorResult::PrepareInstance {
                    startup_deadline_seconds,
                })
            }
            ExecutorActionKind::StartInstance(input) => {
                if input.recipe_id != self.engine_policy.config().id {
                    return Err(ErrorCode::BadRequest);
                }
                let spec = build_generic_container_spec(&self.engine_policy, &input)
                    .map_err(|()| ErrorCode::BadRequest)?;
                let observed = self
                    .runtime
                    .start(spec)
                    .await
                    .map_err(|()| ErrorCode::NotReady)?;
                if let Some(resources) = &self.resources {
                    resources.observe_engine(&observed, super::resources::EnginePhase::Starting);
                }
                Ok(ExecutorResult::StartInstance { observed })
            }
            ExecutorActionKind::PromoteRestartPolicy(input) => {
                let observed = self
                    .runtime
                    .promote_restart(input)
                    .await
                    .map_err(|()| ErrorCode::NotReady)?;
                if let Some(resources) = &self.resources {
                    resources.observe_engine(&observed, super::resources::EnginePhase::Healthy);
                }
                Ok(ExecutorResult::PromoteRestartPolicy { observed })
            }
            ExecutorActionKind::DisableRestartPolicy(input) => self
                .runtime
                .disable_restart(input)
                .await
                .map(|observed| ExecutorResult::DisableRestartPolicy { observed })
                .map_err(|()| ErrorCode::NotReady),
            ExecutorActionKind::StopInstance(input) => {
                self.runtime
                    .stop(input.clone())
                    .await
                    .map_err(|()| ErrorCode::NotReady)?;
                if let Some(resources) = &self.resources {
                    resources.forget_engine(&input.instance_id, input.generation);
                }
                Ok(ExecutorResult::StopInstance)
            }
            ExecutorActionKind::InspectInstance(input) => self
                .runtime
                .inspect(input)
                .await
                .map(|observed| ExecutorResult::InspectInstance { observed })
                .map_err(|()| ErrorCode::NotReady),
            ExecutorActionKind::ReadManagedLogs(input) => self
                .runtime
                .logs(input)
                .await
                .map(|logs| ExecutorResult::ReadManagedLogs { logs })
                .map_err(|()| ErrorCode::NotReady),
            ExecutorActionKind::ReconcileScan(expected) => self
                .reconcile_scan(expected)
                .await
                .map(|scan| ExecutorResult::ReconcileScan { scan })
                .map_err(|()| ErrorCode::NotReady),
        }
    }

    async fn reconcile_scan(
        &self,
        expected: Vec<ReconcileExpectation>,
    ) -> Result<ReconcileScan, ()> {
        if expected.len() > MAX_MANAGED_CONTAINERS {
            return Err(());
        }
        let mut catalogued = BTreeMap::new();
        for item in expected {
            let identity_valid = if item.recipe_id == self.engine_policy.config().id {
                valid_model_repository(&item.model_repository).is_ok()
                    && valid_commit(&item.model_commit)
            } else {
                self.catalog
                    .recipe(&item.recipe_id)
                    .is_some_and(|recipe| recipe.model.commits.contains(&item.model_commit))
            };
            if !valid_reconcile_expectation(&item)
                || !identity_valid
                || catalogued
                    .insert((item.instance_id.clone(), item.generation), item)
                    .is_some()
            {
                return Err(());
            }
        }
        let observations = self.runtime.scan_managed().await?;
        let mut candidates: BTreeMap<(String, u64), Vec<ManagedContainerObservation>> =
            BTreeMap::new();
        let mut quarantined = Vec::new();
        for observation in observations {
            let identity = observation.instance_id.clone().zip(observation.generation);
            let valid = identity
                .as_ref()
                .and_then(|identity| catalogued.get(identity))
                .is_some_and(|expected| {
                    if expected.recipe_id == self.engine_policy.config().id {
                        observation_matches_engine(&observation, expected, &self.engine_policy)
                    } else {
                        self.catalog
                            .recipe(&expected.recipe_id)
                            .is_some_and(|recipe| {
                                observation_matches_expected(&observation, expected, recipe)
                            })
                    }
                });
            if valid {
                if let Some(identity) = identity {
                    candidates.entry(identity).or_default().push(observation);
                }
            } else {
                self.runtime
                    .quarantine(observation.container_id.clone())
                    .await?;
                quarantined.push(QuarantinedContainer {
                    container_id: observation.container_id,
                    instance_id: observation.instance_id,
                    generation: observation.generation,
                    cause: "untrusted-label-generation-network-or-catalog".into(),
                });
            }
        }
        let mut matched = Vec::new();
        for (identity, containers) in candidates {
            if containers.len() != 1 {
                for container in containers {
                    self.runtime
                        .quarantine(container.container_id.clone())
                        .await?;
                    quarantined.push(QuarantinedContainer {
                        container_id: container.container_id,
                        instance_id: container.instance_id,
                        generation: container.generation,
                        cause: "duplicate-managed-generation".into(),
                    });
                }
                continue;
            }
            let container = &containers[0];
            let input = StopInstanceInput {
                instance_id: identity.0.clone(),
                generation: identity.1,
                grace_seconds: 0,
            };
            match self.runtime.inspect(input).await? {
                Some(mut observed) if observed.container_id == container.container_id => {
                    let expected = catalogued.get(&identity).ok_or(())?;
                    if expected.recipe_id == self.engine_policy.config().id {
                        enrich_observed_from_engine(
                            &mut observed,
                            &self.engine_policy,
                            &expected.model_repository,
                            &expected.model_commit,
                        );
                    } else {
                        let recipe = self.catalog.recipe(&expected.recipe_id).ok_or(())?;
                        enrich_observed_from_recipe(&mut observed, recipe);
                    }
                    matched.push(observed)
                }
                _ => {
                    self.runtime
                        .quarantine(container.container_id.clone())
                        .await?;
                    quarantined.push(QuarantinedContainer {
                        container_id: container.container_id.clone(),
                        instance_id: container.instance_id.clone(),
                        generation: container.generation,
                        cause: "stale-or-ambiguous-container-endpoint".into(),
                    });
                }
            }
        }
        let matched_keys = matched
            .iter()
            .map(|engine| (engine.instance_id.clone(), engine.generation))
            .collect::<std::collections::BTreeSet<_>>();
        let missing = catalogued
            .into_iter()
            .filter_map(|(identity, expected)| {
                (!matched_keys.contains(&identity)).then_some(expected)
            })
            .collect();
        Ok(ReconcileScan {
            matched,
            missing,
            quarantined,
        })
    }

    fn inspect_host(&self) -> Result<ProtectedHostSnapshot, ()> {
        let hostname = read_trimmed(&self.hostname_path)?;
        let kernel_release = self.kernel_release.clone();
        let machine_id = read_trimmed(&self.machine_id_path)?;
        let identity_sha256 = format!(
            "{:x}",
            Sha256::digest(format!(
                "{hostname}\0{kernel_release}\0{}\0{machine_id}",
                std::env::consts::ARCH
            ))
        );
        Ok(ProtectedHostSnapshot {
            schema: "sy.spark.executor.host/v1".into(),
            hostname,
            kernel_release,
            architecture: std::env::consts::ARCH.into(),
            identity_sha256,
        })
    }

    async fn dispatch(&self, req: Request) -> Response {
        if req.method == "system.cancel" {
            return self.cancel(&req);
        }
        if req.method != EXECUTOR_METHOD {
            return error_response(
                req.request_id,
                ErrorCode::BadRequest,
                "unknown executor method",
            );
        }
        let action: ExecutorAction = match serde_json::from_value(req.params.clone()) {
            Ok(action) => action,
            Err(_) => {
                return error_response(
                    req.request_id,
                    ErrorCode::BadRequest,
                    "invalid executor action",
                )
            }
        };
        let Some(deadline_ms) = req
            .deadline_ms
            .filter(|value| (1..=MAX_DEADLINE_MS).contains(value))
        else {
            return error_response(req.request_id, ErrorCode::BadRequest, "invalid deadline");
        };
        let deadline = Duration::from_millis(deadline_ms);
        let guard = self.cancellation.register(req.request_id);
        match tokio::time::timeout(
            deadline,
            dispatch_with_cancel(guard, |token| self.execute(action, token)),
        )
        .await
        {
            Ok(Dispatched::Completed(Ok(result))) => ok_response(req.request_id, result),
            Ok(Dispatched::Completed(Err(code))) => executor_error(req.request_id, code),
            Ok(Dispatched::Cancelled) => error_response(
                req.request_id,
                ErrorCode::Cancelled,
                "executor request cancelled",
            ),
            Err(_) => error_response(
                req.request_id,
                ErrorCode::Timeout,
                "executor request timed out",
            ),
        }
    }

    fn cancel(&self, req: &Request) -> Response {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CancelParams {
            target_request_id: Ulid,
        }
        match serde_json::from_value::<CancelParams>(req.params.clone()) {
            Ok(params) => ok_response(
                req.request_id,
                serde_json::json!({
                    "target_request_id": params.target_request_id,
                    "cancelled": self.cancellation.cancel(params.target_request_id),
                }),
            ),
            Err(_) => error_response(
                req.request_id,
                ErrorCode::BadRequest,
                "invalid cancellation",
            ),
        }
    }
}

fn enrich_observed_from_recipe(observed: &mut ObservedEngine, recipe: &super::recipe::Recipe) {
    observed.port = recipe.gateway.port;
    observed.health_method = recipe.health.method.clone();
    observed.health_path = recipe.health.path.clone();
    observed.allowed_routes =
        std::iter::once((recipe.health.method.clone(), recipe.health.path.clone()))
            .chain(
                recipe
                    .gateway
                    .methods
                    .iter()
                    .map(|route| (route.method.clone(), route.path.clone())),
            )
            .collect();
    observed.gateway_profile = recipe.gateway.profile();
    observed.served_model = recipe.health.served_model.clone();
    observed.semantic_prompt = recipe.health.semantic_prompt.clone();
    observed.semantic_max_tokens = recipe.health.semantic_max_tokens;
    observed.startup_deadline_seconds = recipe.health.startup_deadline_seconds;
}

fn enrich_observed_from_engine(
    observed: &mut ObservedEngine,
    policy: &EnginePolicy,
    repository: &str,
    commit: &str,
) {
    let config = policy.config();
    let model_type = read_model_type(config, repository, commit).ok().flatten();
    observed.port = config.port;
    observed.health_method = config.health_method.clone();
    observed.health_path = config.health_path.clone();
    observed.allowed_routes =
        std::iter::once((config.health_method.clone(), config.health_path.clone()))
            .chain(
                config
                    .routes
                    .iter()
                    .map(|route| (route.method.clone(), route.path.clone())),
            )
            .collect();
    observed.gateway_profile = policy.gateway_profile(model_type.as_deref());
    observed.served_model = repository.rsplit('/').next().unwrap_or(repository).into();
    observed.semantic_prompt = config.semantic_prompt.clone();
    observed.semantic_max_tokens = config.semantic_max_tokens;
    observed.startup_deadline_seconds = config.startup_deadline_seconds;
}

fn valid_reconcile_expectation(expected: &ReconcileExpectation) -> bool {
    expected.instance_id.len() == 34
        && expected.instance_id.starts_with("i_")
        && expected.instance_id[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && expected.generation > 0
        && expected.model_commit.len() == 40
        && expected.recipe_id.len() <= 128
}

fn observation_matches_expected(
    observed: &ManagedContainerObservation,
    expected: &ReconcileExpectation,
    recipe: &super::recipe::Recipe,
) -> bool {
    observed.container_id.len() == 64
        && observed
            .container_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && observed.name == format!("sy-spark-{}-g{}", expected.instance_id, expected.generation)
        && observed.instance_id.as_deref() == Some(expected.instance_id.as_str())
        && observed.generation == Some(expected.generation)
        && observed.role.as_deref() == Some("engine")
        && observed.model_commit.as_deref() == Some(expected.model_commit.as_str())
        && observed.recipe_id.as_deref() == Some(expected.recipe_id.as_str())
        && observed.image.as_deref()
            == Some(
                format!(
                    "{}@{}",
                    recipe.engine.image_repository, recipe.engine.image_digest
                )
                .as_str(),
            )
        && observed.networks.len() == 1
        && observed.restart_policy.len() <= 32
}

fn observation_matches_engine(
    observed: &ManagedContainerObservation,
    expected: &ReconcileExpectation,
    policy: &EnginePolicy,
) -> bool {
    observed.container_id.len() == 64
        && observed
            .container_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && observed.name == format!("sy-spark-{}-g{}", expected.instance_id, expected.generation)
        && observed.instance_id.as_deref() == Some(expected.instance_id.as_str())
        && observed.generation == Some(expected.generation)
        && observed.role.as_deref() == Some("engine")
        && observed.model_commit.as_deref() == Some(expected.model_commit.as_str())
        && observed.model_repository.as_deref() == Some(expected.model_repository.as_str())
        && observed.recipe_id.as_deref() == Some(expected.recipe_id.as_str())
        && observed.image.as_deref() == Some(policy.image().as_str())
        && observed.networks == [policy.config().network.as_str()]
        && observed.restart_policy.len() <= 32
}

impl Handler for ExecutorHandler {
    async fn handle(&self, req: Request) -> Response {
        self.dispatch(req).await
    }
}

fn read_trimmed(path: &Path) -> Result<String, ()> {
    std::fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .map_err(|_| ())
        .and_then(|value| if value.is_empty() { Err(()) } else { Ok(value) })
}

fn ok_response(request_id: Ulid, value: impl Serialize) -> Response {
    match serde_json::to_value(value) {
        Ok(result) => Response::Ok {
            schema_version: SCHEMA_VERSION,
            request_id,
            result,
            blob: None,
        },
        Err(_) => executor_error(request_id, ErrorCode::Internal),
    }
}

fn executor_error(request_id: Ulid, code: ErrorCode) -> Response {
    let message = match code {
        ErrorCode::NotReady => "executor dependency unavailable",
        ErrorCode::Internal => "executor inspection failed",
        _ => "executor request failed",
    };
    error_response(request_id, code, message)
}

fn error_response(request_id: Ulid, code: ErrorCode, message: &str) -> Response {
    Response::Err {
        schema_version: SCHEMA_VERSION,
        request_id,
        error: ErrorBody {
            code,
            message: message.into(),
            retry_after_ms: None,
            details: serde_json::Value::Null,
        },
    }
}

#[derive(Debug, Clone)]
pub struct ExecutorClient {
    socket: PathBuf,
    deadline: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorClientError {
    pub code: ErrorCode,
    pub detail: &'static str,
}

pub struct PreparedStart {
    input: StartInstanceInput,
    deadline: Duration,
}

impl ExecutorClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            deadline: Duration::from_secs(2),
        }
    }

    pub async fn snapshot(&self) -> Result<ExecutorSnapshot, ExecutorClientError> {
        let health = self.health().await?;
        let host = match self
            .call(ExecutorAction {
                action: ExecutorActionKind::InspectProtectedHost,
            })
            .await?
        {
            ExecutorResult::InspectProtectedHost { host } => host,
            _ => return Err(protocol_error()),
        };
        let docker = match self
            .call(ExecutorAction {
                action: ExecutorActionKind::InspectDockerVersion,
            })
            .await?
        {
            ExecutorResult::InspectDockerVersion { docker } => docker,
            _ => return Err(protocol_error()),
        };
        let (resources, resource_policy) = match self
            .call(ExecutorAction {
                action: ExecutorActionKind::InspectResources,
            })
            .await?
        {
            ExecutorResult::InspectResources { snapshot, policy } => (snapshot, policy),
            _ => return Err(protocol_error()),
        };
        Ok(ExecutorSnapshot {
            health,
            host,
            docker,
            resources,
            resource_policy,
        })
    }

    pub async fn health(&self) -> Result<ExecutorHealth, ExecutorClientError> {
        match self
            .call(ExecutorAction {
                action: ExecutorActionKind::Health,
            })
            .await?
        {
            ExecutorResult::Health { health } => Ok(health),
            _ => Err(protocol_error()),
        }
    }

    pub async fn emergency_records(
        &self,
    ) -> Result<Vec<super::resources::EmergencyRecord>, ExecutorClientError> {
        match self
            .call(ExecutorAction {
                action: ExecutorActionKind::InspectEmergencyRecords,
            })
            .await?
        {
            ExecutorResult::InspectEmergencyRecords { records } => Ok(records),
            _ => Err(protocol_error()),
        }
    }

    pub async fn prepare_instance(
        &self,
        input: StartInstanceInput,
    ) -> Result<PreparedStart, ExecutorClientError> {
        match self
            .call_with_timeout(
                ExecutorAction {
                    action: ExecutorActionKind::PrepareInstance(input.clone()),
                },
                Duration::from_millis(MAX_DEADLINE_MS),
            )
            .await?
        {
            ExecutorResult::PrepareInstance {
                startup_deadline_seconds,
            } if (1..=super::recipe::MAX_STARTUP_DEADLINE_SECONDS)
                .contains(&startup_deadline_seconds) =>
            {
                Ok(PreparedStart {
                    input,
                    deadline: Duration::from_secs(startup_deadline_seconds),
                })
            }
            _ => Err(protocol_error()),
        }
    }

    pub async fn start_prepared(
        &self,
        prepared: PreparedStart,
    ) -> Result<ObservedEngine, ExecutorClientError> {
        match self
            .call_with_timeout(
                ExecutorAction {
                    action: ExecutorActionKind::StartInstance(prepared.input),
                },
                prepared.deadline,
            )
            .await?
        {
            ExecutorResult::StartInstance { observed } => Ok(observed),
            _ => Err(protocol_error()),
        }
    }

    pub async fn promote_restart(
        &self,
        input: StopInstanceInput,
    ) -> Result<ObservedEngine, ExecutorClientError> {
        match self
            .call_with_timeout(
                ExecutorAction {
                    action: ExecutorActionKind::PromoteRestartPolicy(input),
                },
                Duration::from_secs(10),
            )
            .await?
        {
            ExecutorResult::PromoteRestartPolicy { observed } => Ok(observed),
            _ => Err(protocol_error()),
        }
    }

    pub async fn disable_restart(
        &self,
        input: StopInstanceInput,
    ) -> Result<ObservedEngine, ExecutorClientError> {
        match self
            .call_with_timeout(
                ExecutorAction {
                    action: ExecutorActionKind::DisableRestartPolicy(input),
                },
                Duration::from_secs(10),
            )
            .await?
        {
            ExecutorResult::DisableRestartPolicy { observed } => Ok(observed),
            _ => Err(protocol_error()),
        }
    }

    pub async fn stop_instance(&self, input: StopInstanceInput) -> Result<(), ExecutorClientError> {
        match self
            .call_with_timeout(
                ExecutorAction {
                    action: ExecutorActionKind::StopInstance(input),
                },
                Duration::from_secs(30),
            )
            .await?
        {
            ExecutorResult::StopInstance => Ok(()),
            _ => Err(protocol_error()),
        }
    }

    pub async fn logs(&self, input: LogInput) -> Result<EngineLogs, ExecutorClientError> {
        match self
            .call_with_timeout(
                ExecutorAction {
                    action: ExecutorActionKind::ReadManagedLogs(input),
                },
                Duration::from_secs(10),
            )
            .await?
        {
            ExecutorResult::ReadManagedLogs { logs } => Ok(logs),
            _ => Err(protocol_error()),
        }
    }

    pub async fn reconcile_scan(
        &self,
        expected: Vec<ReconcileExpectation>,
    ) -> Result<ReconcileScan, ExecutorClientError> {
        match self
            .call_with_timeout(
                ExecutorAction {
                    action: ExecutorActionKind::ReconcileScan(expected),
                },
                Duration::from_secs(30),
            )
            .await?
        {
            ExecutorResult::ReconcileScan { scan } => Ok(scan),
            _ => Err(protocol_error()),
        }
    }

    async fn call(&self, action: ExecutorAction) -> Result<ExecutorResult, ExecutorClientError> {
        self.call_with_timeout(action, self.deadline).await
    }

    async fn call_with_timeout(
        &self,
        action: ExecutorAction,
        deadline: Duration,
    ) -> Result<ExecutorResult, ExecutorClientError> {
        let mut client = tokio::time::timeout(deadline, sy_ipc::Client::connect(&self.socket))
            .await
            .map_err(|_| unavailable_error())?
            .map_err(|_| unavailable_error())?;
        let response = tokio::time::timeout(
            deadline,
            client.call(
                EXECUTOR_METHOD,
                serde_json::to_value(action).map_err(|_| protocol_error())?,
                CallOpts {
                    priority: Priority::Interactive,
                    deadline_ms: Some(deadline.as_millis() as u64),
                    ..CallOpts::default()
                },
            ),
        )
        .await
        .map_err(|_| unavailable_error())?
        .map_err(|_| unavailable_error())?;
        match response {
            Response::Ok { result, .. } => {
                serde_json::from_value(result).map_err(|_| protocol_error())
            }
            Response::Err { error, .. } => Err(ExecutorClientError {
                code: error.code,
                detail: "executor rejected the typed request",
            }),
        }
    }
}

fn unavailable_error() -> ExecutorClientError {
    ExecutorClientError {
        code: ErrorCode::NotReady,
        detail: "executor unavailable",
    }
}

fn protocol_error() -> ExecutorClientError {
    ExecutorClientError {
        code: ErrorCode::IncompatibleSchema,
        detail: "executor protocol mismatch",
    }
}

pub async fn serve(config_path: &Path) -> anyhow::Result<()> {
    let config: ExecutorConfig = toml::from_str(&tokio::fs::read_to_string(config_path).await?)?;
    anyhow::ensure!(
        config.schema == "sy.spark.executor/v1",
        "unsupported Spark executor configuration schema"
    );
    anyhow::ensure!(
        config.socket.is_absolute(),
        "executor socket must be absolute"
    );
    anyhow::ensure!(
        config.socket == Path::new(EXECUTOR_SOCKET),
        "executor socket must use the fixed application path"
    );
    if let Ok(metadata) = std::fs::symlink_metadata(&config.socket) {
        anyhow::ensure!(
            metadata.file_type().is_socket(),
            "executor socket path is not a socket"
        );
        std::fs::remove_file(&config.socket)?;
    }
    let listener = UnixListener::bind(&config.socket)?;
    std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o660))?;
    let heartbeats = Arc::new(Heartbeats::default());
    tokio::spawn(docker_event_loop(heartbeats.clone()));
    anyhow::ensure!(
        config.recipes_dir.is_absolute(),
        "recipe catalog path must be absolute"
    );
    anyhow::ensure!(
        config.recipes_dir == Path::new(RECIPE_CATALOG_DIR),
        "recipe catalog must use the fixed root-owned path"
    );
    anyhow::ensure!(
        config.resources_policy == Path::new(RESOURCE_POLICY_PATH),
        "resource policy must use the fixed root-owned path"
    );
    anyhow::ensure!(
        config.engine_policy == Path::new(ENGINE_POLICY_PATH),
        "engine policy must use the fixed root-owned path"
    );
    let resource_policy =
        load_resource_policy(&config.resources_policy).map_err(anyhow::Error::msg)?;
    let resources = Arc::new(ResourceMonitor::new(
        Box::new(ProcfsHostSampler::production()),
        resource_policy,
        heartbeats.clone(),
        "/var/lib/sy-spark/executor/emergency.jsonl".into(),
    ));
    resources.sample_once();
    resources.sample_once();
    let runtime: Arc<dyn ContainerRuntime> = Arc::new(BollardContainerRuntime);
    tokio::spawn(resource_sampling_loop(resources.clone(), runtime.clone()));
    anyhow::ensure!(
        config.host.architecture == std::env::consts::ARCH,
        "configured recipe host architecture differs from the running executor"
    );
    anyhow::ensure!(
        config
            .host
            .protected_fingerprint
            .strip_prefix("sha256:")
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }),
        "configured recipe host fingerprint is invalid"
    );
    let handler = ExecutorHandler::production(&config, heartbeats.clone(), resources, runtime)
        .map_err(anyhow::Error::msg)?;
    let server = Server::with_authorizer(handler, SparkUidAuthorizer::new(config.agent_uid))
        .with_max_request_frame_length(MAX_REQUEST_FRAME);
    sy_core::notify::ready();
    let _watchdog = spawn_heartbeat_watchdog(heartbeats);
    server.serve(listener).await?;
    Ok(())
}

fn spawn_heartbeat_watchdog(heartbeats: Arc<Heartbeats>) -> Option<std::thread::JoinHandle<()>> {
    let interval = sd_notify::watchdog_enabled()? / 2;
    Some(std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        let (guard, events) = heartbeats.healthy();
        if !guard || !events {
            tracing::error!(
                target: "sy::spark::executor",
                guard_heartbeat = guard,
                event_heartbeat = events,
                "executor safety heartbeat stalled"
            );
            return;
        }
        if sd_notify::notify(&[sd_notify::NotifyState::Watchdog]).is_err() {
            return;
        }
    }))
}

async fn docker_event_loop(heartbeats: Arc<Heartbeats>) {
    let mut since = now_seconds().saturating_sub(1);
    loop {
        let Ok(docker) = BollardContainerRuntime::docker().await else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        let until = now_seconds().saturating_add(1);
        let events = docker.events(Some(
            EventsOptionsBuilder::default()
                .since(&since.to_string())
                .until(&until.to_string())
                .filters(&managed_event_filters())
                .build(),
        ));
        match tokio::time::timeout(
            Duration::from_secs(3),
            events
                .take(MAX_MANAGED_EVENTS_PER_WINDOW + 1)
                .try_collect::<Vec<_>>(),
        )
        .await
        {
            Ok(Ok(events)) if events.len() <= MAX_MANAGED_EVENTS_PER_WINDOW => {
                for event in events {
                    observe_managed_event(&heartbeats, &event);
                }
                heartbeats.mark_events();
                since = until;
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

fn observe_managed_event(heartbeats: &Heartbeats, event: &EventMessage) {
    if managed_event_is_relevant(event) {
        heartbeats.wake_for_managed_event();
    }
}

fn managed_event_filters() -> std::collections::HashMap<String, Vec<String>> {
    std::collections::HashMap::from([
        ("type".into(), vec!["container".into()]),
        ("label".into(), vec!["io.sy.spark.managed=true".into()]),
    ])
}

fn managed_event_is_relevant(event: &EventMessage) -> bool {
    const ACTIONS: [&str; 14] = [
        "create",
        "start",
        "die",
        "stop",
        "destroy",
        "kill",
        "oom",
        "pause",
        "unpause",
        "restart",
        "update",
        "rename",
        "health_status: healthy",
        "health_status: unhealthy",
    ];
    if event.typ != Some(EventMessageTypeEnum::CONTAINER)
        || !event
            .action
            .as_deref()
            .is_some_and(|action| ACTIONS.contains(&action))
    {
        return false;
    }
    let Some(actor) = &event.actor else {
        return false;
    };
    let valid_id = actor.id.as_ref().is_some_and(|id| {
        id.len() == 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    });
    let Some(attributes) = actor
        .attributes
        .as_ref()
        .filter(|values| values.len() <= 32)
    else {
        return false;
    };
    let bounded = attributes
        .iter()
        .all(|(key, value)| key.len() <= 128 && value.len() <= 256);
    let instance = attributes.get("io.sy.spark.instance");
    valid_id
        && bounded
        && attributes.get("io.sy.spark.managed").map(String::as_str) == Some("true")
        && attributes.get("io.sy.spark.role").map(String::as_str) == Some("engine")
        && instance.is_some_and(|id| {
            id.len() == 34
                && id.starts_with("i_")
                && id[2..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        && attributes
            .get("io.sy.spark.generation")
            .and_then(|generation| generation.parse::<u64>().ok())
            .is_some_and(|generation| generation > 0)
}

async fn resource_sampling_loop(
    resources: Arc<ResourceMonitor>,
    runtime: Arc<dyn ContainerRuntime>,
) {
    loop {
        resources.sample_once();
        if let Some(decision) = resources.take_emergency() {
            if actuate_emergency(&resources, runtime.as_ref(), &decision).await {
                resources.suppress(&decision);
            }
        }
        tokio::time::sleep(Duration::from_millis(resources.sampling_interval_ms())).await;
    }
}

async fn actuate_emergency(
    resources: &ResourceMonitor,
    runtime: &dyn ContainerRuntime,
    decision: &super::resources::EmergencyDecision,
) -> bool {
    actuate_emergency_at(resources, runtime, decision, Path::new("/sys/fs/cgroup")).await
}

async fn actuate_emergency_at(
    resources: &ResourceMonitor,
    runtime: &dyn ContainerRuntime,
    decision: &super::resources::EmergencyDecision,
    cgroup_root: &Path,
) -> bool {
    let Some(expected) = resources.identity(decision) else {
        return false;
    };
    let observed = runtime
        .disable_restart(StopInstanceInput {
            instance_id: decision.instance_id.clone(),
            generation: decision.generation,
            grace_seconds: 0,
        })
        .await;
    let Ok(observed) = observed else {
        return false;
    };
    let fresh = super::resources::ManagedCgroupIdentity {
        managed_label: true,
        engine_role: "engine".into(),
        instance_id: observed.instance_id,
        generation: observed.generation,
        container_id: observed.container_id,
        init_pid: observed.init_pid,
        pid_start_time_ticks: observed.pid_start_time_ticks,
        cgroup_path: observed.cgroup_path,
    };
    (resources._cgroup_kill)(cgroup_root, &expected, &fresh).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FakeRuntime {
        calls: Arc<Mutex<Vec<String>>>,
        stops: Arc<Mutex<Vec<StopInstanceInput>>>,
        managed: Arc<Mutex<Vec<ManagedContainerObservation>>>,
        observed: ObservedEngine,
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                stops: Arc::new(Mutex::new(Vec::new())),
                managed: Arc::new(Mutex::new(Vec::new())),
                observed: observed_engine(),
            }
        }

        fn record(&self, call: impl Into<String>) {
            self.calls.lock().unwrap().push(call.into());
        }
    }

    impl ContainerRuntime for FakeRuntime {
        fn ensure_network(&self, _: String) -> RuntimeFuture<String> {
            self.record("network");
            Box::pin(async { Ok("network-id".into()) })
        }

        fn ensure_image(&self, image: String, _: String) -> RuntimeFuture<()> {
            self.record(format!("image:{image}"));
            Box::pin(async { Ok(()) })
        }

        fn start(&self, _: ContainerSpec) -> RuntimeFuture<ObservedEngine> {
            self.record("start");
            let observed = self.observed.clone();
            Box::pin(async move { Ok(observed) })
        }

        fn promote_restart(&self, _: StopInstanceInput) -> RuntimeFuture<ObservedEngine> {
            self.record("promote");
            let mut observed = self.observed.clone();
            observed.restart_policy = "unless-stopped".into();
            Box::pin(async move { Ok(observed) })
        }

        fn disable_restart(&self, _: StopInstanceInput) -> RuntimeFuture<ObservedEngine> {
            self.record("disable");
            let observed = self.observed.clone();
            Box::pin(async move { Ok(observed) })
        }

        fn inspect(&self, _: StopInstanceInput) -> RuntimeFuture<Option<ObservedEngine>> {
            self.record("inspect");
            let observed = self.observed.clone();
            Box::pin(async move { Ok(Some(observed)) })
        }

        fn stop(&self, input: StopInstanceInput) -> RuntimeFuture<()> {
            self.record("stop");
            self.stops.lock().unwrap().push(input);
            Box::pin(async { Ok(()) })
        }

        fn logs(&self, input: LogInput) -> RuntimeFuture<EngineLogs> {
            self.record("logs");
            Box::pin(async move {
                Ok(EngineLogs {
                    cursor: input.cursor,
                    next_cursor: input.cursor + 1,
                    truncated: false,
                    lines: vec!["fixture ok".into()],
                })
            })
        }

        fn scan_managed(&self) -> RuntimeFuture<Vec<ManagedContainerObservation>> {
            self.record("scan");
            let managed = self.managed.lock().unwrap().clone();
            Box::pin(async move { Ok(managed) })
        }

        fn quarantine(&self, _: String) -> RuntimeFuture<()> {
            self.record("quarantine");
            Box::pin(async { Ok(()) })
        }
    }

    fn observed_engine() -> ObservedEngine {
        ObservedEngine {
            instance_id: format!("i_{}", "1".repeat(32)),
            generation: 1,
            container_id: "a".repeat(64),
            network_id: "network-id".into(),
            address: "172.30.0.2".into(),
            port: 8000,
            running: true,
            restart_policy: "no".into(),
            health_method: "GET".into(),
            health_path: "/health".into(),
            allowed_routes: vec![("GET".into(), "/health".into())],
            gateway_profile: GatewayProfile::text(),
            served_model: "fixture".into(),
            semantic_prompt: "OK".into(),
            semantic_max_tokens: 1,
            startup_deadline_seconds: 30,
            init_pid: 123,
            pid_start_time_ticks: 456,
            cgroup_path: format!("system.slice/docker-{}.scope", "a".repeat(64)),
        }
    }

    fn managed_event(action: &str) -> EventMessage {
        EventMessage {
            typ: Some(EventMessageTypeEnum::CONTAINER),
            action: Some(action.into()),
            actor: Some(bollard::models::EventActor {
                id: Some("a".repeat(64)),
                attributes: Some(std::collections::HashMap::from([
                    ("io.sy.spark.managed".into(), "true".into()),
                    ("io.sy.spark.role".into(), "engine".into()),
                    (
                        "io.sy.spark.instance".into(),
                        format!("i_{}", "1".repeat(32)),
                    ),
                    ("io.sy.spark.generation".into(), "1".into()),
                ])),
            }),
            ..Default::default()
        }
    }

    struct FakeImageStore {
        inspections: Mutex<std::collections::VecDeque<Result<ImageInspect, ()>>>,
        pulls: AtomicU64,
        pull_succeeds: bool,
    }

    impl ImageStore for FakeImageStore {
        fn inspect_exact(&self, _: String) -> RuntimeFuture<ImageInspect> {
            let inspected = self.inspections.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { inspected })
        }

        fn pull_exact(&self, _: String) -> RuntimeFuture<()> {
            self.pulls.fetch_add(1, Ordering::SeqCst);
            let succeeds = self.pull_succeeds;
            Box::pin(async move { succeeds.then_some(()).ok_or(()) })
        }
    }

    #[tokio::test]
    async fn exact_local_image_is_idempotent_without_another_pull() {
        const IMAGE: &str = "vllm/vllm-openai@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let store = FakeImageStore {
            inspections: Mutex::new(std::collections::VecDeque::from([Ok(ImageInspect {
                repo_digests: Some(vec![IMAGE.into()]),
                architecture: Some("arm64".into()),
                ..Default::default()
            })])),
            pulls: AtomicU64::new(0),
            pull_succeeds: false,
        };

        assert!(ensure_exact_image(&store, IMAGE, "arm64").await.is_ok());
        assert_eq!(store.pulls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn pull_stream_error_succeeds_only_when_exact_image_is_locally_verified() {
        const IMAGE: &str = "vllm/vllm-openai@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let exact = ImageInspect {
            repo_digests: Some(vec![IMAGE.into()]),
            architecture: Some("arm64".into()),
            ..Default::default()
        };
        let recovered = FakeImageStore {
            inspections: Mutex::new(std::collections::VecDeque::from([Err(()), Ok(exact)])),
            pulls: AtomicU64::new(0),
            pull_succeeds: false,
        };
        assert!(ensure_exact_image(&recovered, IMAGE, "arm64").await.is_ok());
        assert_eq!(recovered.pulls.load(Ordering::SeqCst), 1);

        let mismatched = FakeImageStore {
            inspections: Mutex::new(std::collections::VecDeque::from([
                Err(()),
                Ok(ImageInspect {
                    repo_digests: Some(vec![IMAGE.into()]),
                    architecture: Some("amd64".into()),
                    ..Default::default()
                }),
            ])),
            pulls: AtomicU64::new(0),
            pull_succeeds: false,
        };
        assert!(ensure_exact_image(&mismatched, IMAGE, "arm64")
            .await
            .is_err());
    }

    #[test]
    fn managed_docker_event_filter_is_strict_and_relevant_event_advances_epoch() {
        assert_eq!(
            managed_event_filters(),
            std::collections::HashMap::from([
                ("type".into(), vec!["container".into()]),
                ("label".into(), vec!["io.sy.spark.managed=true".into()]),
            ])
        );
        let heartbeats = Heartbeats::default();
        observe_managed_event(&heartbeats, &managed_event("die"));
        assert_eq!(heartbeats.event_epoch(), 1);
    }

    #[test]
    fn unrelated_or_malformed_docker_events_never_wake_reconciliation() {
        let heartbeats = Heartbeats::default();
        observe_managed_event(&heartbeats, &managed_event("exec_start"));
        let mut malformed = managed_event("start");
        malformed
            .actor
            .as_mut()
            .unwrap()
            .attributes
            .as_mut()
            .unwrap()
            .insert("io.sy.spark.generation".into(), "0".into());
        observe_managed_event(&heartbeats, &malformed);
        assert_eq!(heartbeats.event_epoch(), 0);
    }

    #[test]
    fn event_heartbeat_is_truthful_about_subscription_liveness() {
        let heartbeats = Heartbeats::default();
        heartbeats.mark_events();
        assert!(heartbeats.healthy().1);
        heartbeats.events.store(
            now_seconds().saturating_sub(HEARTBEAT_STALE_SECONDS + 1),
            Ordering::Release,
        );
        assert!(!heartbeats.healthy().1);
    }

    struct StaticSampler(Result<HostResourceSnapshot, super::super::resources::SampleError>);

    impl HostSampler for StaticSampler {
        fn sample(&mut self) -> Result<HostResourceSnapshot, super::super::resources::SampleError> {
            self.0.clone()
        }
    }

    fn recipe_host() -> RecipeHost {
        RecipeHost {
            architecture: "aarch64".into(),
            gpu_model: "NVIDIA GB10".into(),
            compute_capability: "12.1".into(),
            dgx_build: "7.5.0".into(),
            driver_version: "580.159.03".into(),
            toolkit_version: "1.19.0".into(),
            protected_fingerprint:
                "sha256:7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510".into(),
        }
    }

    fn catalog() -> Arc<RecipeCatalog> {
        Arc::new(RecipeCatalog::signed_for_test())
    }

    fn engine_policy() -> Arc<EnginePolicy> {
        Arc::new(EnginePolicy::parse(include_str!("../../configs/sy/spark/engine.toml")).unwrap())
    }

    fn generic_input(marker: char) -> StartInstanceInput {
        let policy = engine_policy();
        StartInstanceInput {
            instance_id: format!("i_{}", marker.to_string().repeat(32)),
            generation: 1,
            model_commit: marker.to_string().repeat(40),
            model_repository: "unlisted-owner/compatible-model".into(),
            recipe_id: policy.config().id.clone(),
            operation_id: "01K00000000000000000000000".into(),
        }
    }

    #[test]
    fn production_guard_fsyncs_exact_victim_and_faults_closed_without_telemetry() {
        let root = tempfile::tempdir().unwrap();
        let journal = root.path().join("emergency.jsonl");
        let heartbeats = Arc::new(Heartbeats::default());
        let snapshot = HostResourceSnapshot {
            schema: "sy.spark.resources.snapshot/v1".into(),
            observed_at_unix_ms: super::super::resources::unix_millis(),
            mem_total_bytes: Some(64 * super::super::resources::GIB_BYTES),
            mem_available_bytes: Some(7 * super::super::resources::GIB_BYTES),
            memory_full_psi_avg10_percent: Some(0.0),
            swap_in_pages_delta: Some(0),
            disk_available_bytes: Some(200 * super::super::resources::GIB_BYTES),
        };
        let monitor = ResourceMonitor::new(
            Box::new(StaticSampler(Ok(snapshot.clone()))),
            ResourcePolicy::capacity_first(),
            heartbeats,
            journal.clone(),
        );
        monitor
            .managed
            .write()
            .unwrap()
            .push(super::super::resources::ManagedEngine {
                instance_id: "starting".into(),
                generation: 7,
                phase: super::super::resources::EnginePhase::Starting,
                started_sequence: 9,
                memory_bytes: 4,
                previous_memory_bytes: 1,
            });
        assert!(monitor.sample_once());
        assert!(monitor.sample_once());
        assert!(monitor.sample_once());
        let records = super::super::resources::read_emergency_records(&journal).unwrap();
        assert_eq!(
            (records.len(), records[0].decision.instance_id.as_str()),
            (1, "starting")
        );

        let faulted = ResourceMonitor::new(
            Box::new(StaticSampler(Err(
                super::super::resources::SampleError::Unavailable,
            ))),
            ResourcePolicy::capacity_first(),
            Arc::new(Heartbeats::default()),
            root.path().join("faulted.jsonl"),
        );
        assert!(!faulted.sample_once() && faulted.snapshot().is_none());
    }

    struct BlockingDocker {
        started: Arc<tokio::sync::Notify>,
    }

    impl DockerInspector for BlockingDocker {
        fn inspect(&self, cancellation: CancellationToken) -> DockerFuture {
            let started = self.started.clone();
            Box::pin(async move {
                started.notify_one();
                cancellation.cancelled().await;
                Err(())
            })
        }
    }

    #[test]
    fn spark_authorizer_accepts_exact_service_uid_only() {
        const SERVICE_UID: u32 = 996;
        let authorizer = SparkUidAuthorizer::new(SERVICE_UID);
        assert!(authorizer.authorize(Some(PeerCredentials::new(Some(10), SERVICE_UID, 983))));
        assert!(!authorizer.authorize(Some(PeerCredentials::new(Some(1), 0, 983))));
        assert!(!authorizer.authorize(Some(PeerCredentials::new(Some(11), 1000, 983))));
        assert!(!authorizer.authorize(Some(PeerCredentials::new(Some(12), 1001, 983))));
        assert!(!authorizer.authorize(None));
    }

    #[test]
    fn signed_executor_config_pins_catalog_and_exact_host_identity() {
        let config: ExecutorConfig =
            toml::from_str(include_str!("../../configs/sy/spark/executor.toml")).unwrap();
        assert_eq!(config.recipes_dir, Path::new(RECIPE_CATALOG_DIR));
        assert_eq!(config.engine_policy, Path::new(ENGINE_POLICY_PATH));
        assert_eq!(config.resources_policy, Path::new(RESOURCE_POLICY_PATH));
        assert_eq!(config.host.gpu_model, "NVIDIA GB10");
        assert_eq!(config.host.compute_capability, "12.1");
        assert_eq!(config.host.dgx_build, "7.5.0");
    }

    #[test]
    fn protected_host_snapshot_uses_injected_kernel_release() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("hostname"), "spark-test").expect("hostname");
        std::fs::write(root.path().join("machine-id"), "fixture-machine").expect("machine id");
        let handler = ExecutorHandler {
            agent_uid: 996,
            cancellation: Arc::new(CancelRegistry::new()),
            docker: Arc::new(BollardDockerInspector),
            runtime: Arc::new(BollardContainerRuntime),
            heartbeats: Arc::new(Heartbeats::default()),
            hostname_path: root.path().join("hostname"),
            kernel_release: "6.17-test".into(),
            machine_id_path: root.path().join("machine-id"),
            catalog: catalog(),
            engine_policy: engine_policy(),
            recipe_host: recipe_host(),
            resources: None,
        };
        assert_eq!(handler.inspect_host().unwrap().kernel_release, "6.17-test");
        assert!(!include_str!("executor.rs").contains(concat!("/proc/sys/kernel/", "osrelease")));
    }

    #[test]
    fn unknown_action_field_or_oversized_frame_fails_closed() {
        use tokio_util::codec::Decoder;
        let unknown = serde_json::json!({"action":"health", "url":"tcp://attacker"});
        assert!(serde_json::from_value::<ExecutorAction>(unknown).is_err());
        let action = serde_json::to_value(ExecutorAction {
            action: ExecutorActionKind::Health,
        })
        .expect("serialize action");
        assert_eq!(action.as_object().expect("object").len(), 1);
        let oversized = MAX_REQUEST_FRAME + 1;
        let mut frame = bytes::BytesMut::new();
        frame.extend_from_slice(&(oversized as u32).to_be_bytes());
        frame.resize(4 + oversized, b'x');
        let error = sy_ipc::RequestCodec::with_max_frame_length(MAX_REQUEST_FRAME)
            .decode(&mut frame)
            .expect_err("oversized executor frame");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn typed_errors_do_not_expose_dependency_details() {
        let response = executor_error(Ulid::new(), ErrorCode::NotReady);
        let rendered = serde_json::to_string(&response).expect("serialize");
        assert!(!rendered.contains("docker.sock"));
    }

    #[tokio::test]
    async fn cancellation_reaches_the_single_registered_executor_request() {
        let root = tempfile::tempdir().expect("tempdir");
        for (name, value) in [
            ("hostname", "spark-test"),
            ("machine-id", "fixture-machine"),
        ] {
            std::fs::write(root.path().join(name), value).expect("fixture");
        }
        let cancellation = Arc::new(CancelRegistry::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let heartbeats = Arc::new(Heartbeats::default());
        heartbeats.mark_guard();
        heartbeats.mark_events();
        let handler = Arc::new(ExecutorHandler {
            agent_uid: 996,
            cancellation: cancellation.clone(),
            docker: Arc::new(BlockingDocker {
                started: started.clone(),
            }),
            runtime: Arc::new(BollardContainerRuntime),
            heartbeats,
            hostname_path: root.path().join("hostname"),
            kernel_release: "6.17-test".into(),
            machine_id_path: root.path().join("machine-id"),
            catalog: catalog(),
            engine_policy: engine_policy(),
            recipe_host: recipe_host(),
            resources: None,
        });
        let request_id = Ulid::new();
        let request = Request {
            schema_version: SCHEMA_VERSION,
            request_id,
            trace_id: None,
            parent_span_id: None,
            deadline_ms: Some(MAX_DEADLINE_MS),
            priority: Priority::Interactive,
            method: EXECUTOR_METHOD.into(),
            params: serde_json::json!({"action":"inspect_docker_version"}),
        };
        let task = tokio::spawn(async move { handler.dispatch(request).await });
        started.notified().await;
        assert!(cancellation.cancel(request_id));
        match task.await.expect("join") {
            Response::Err { error, .. } => assert_eq!(error.code, ErrorCode::Cancelled),
            response => panic!("expected cancellation error, got {response:?}"),
        }
    }

    #[tokio::test]
    async fn missing_zero_or_excessive_deadlines_fail_closed() {
        let heartbeats = Arc::new(Heartbeats::default());
        heartbeats.mark_guard();
        heartbeats.mark_events();
        let handler = ExecutorHandler {
            agent_uid: 996,
            cancellation: Arc::new(CancelRegistry::new()),
            docker: Arc::new(BollardDockerInspector),
            runtime: Arc::new(BollardContainerRuntime),
            heartbeats,
            hostname_path: "/etc/hostname".into(),
            kernel_release: "6.17-test".into(),
            machine_id_path: "/etc/machine-id".into(),
            catalog: catalog(),
            engine_policy: engine_policy(),
            recipe_host: recipe_host(),
            resources: None,
        };
        for deadline_ms in [None, Some(0), Some(MAX_DEADLINE_MS + 1)] {
            let request = Request {
                schema_version: SCHEMA_VERSION,
                request_id: Ulid::new(),
                trace_id: None,
                parent_span_id: None,
                deadline_ms,
                priority: Priority::Interactive,
                method: EXECUTOR_METHOD.into(),
                params: serde_json::json!({"action":"health"}),
            };
            match handler.dispatch(request).await {
                Response::Err { error, .. } => assert_eq!(error.code, ErrorCode::BadRequest),
                response => panic!("expected deadline rejection, got {response:?}"),
            }
        }
    }

    #[tokio::test]
    async fn exact_recipe_startup_deadline_above_180_seconds_is_accepted() {
        let heartbeats = Arc::new(Heartbeats::default());
        heartbeats.mark_guard();
        heartbeats.mark_events();
        let handler = ExecutorHandler {
            agent_uid: 996,
            cancellation: Arc::new(CancelRegistry::new()),
            docker: Arc::new(BollardDockerInspector),
            runtime: Arc::new(BollardContainerRuntime),
            heartbeats,
            hostname_path: "/etc/hostname".into(),
            kernel_release: "6.17-test".into(),
            machine_id_path: "/etc/machine-id".into(),
            catalog: catalog(),
            engine_policy: engine_policy(),
            recipe_host: recipe_host(),
            resources: None,
        };
        let request_id = Ulid::new();
        let response = handler
            .dispatch(Request {
                schema_version: SCHEMA_VERSION,
                request_id,
                trace_id: None,
                parent_span_id: None,
                method: EXECUTOR_METHOD.into(),
                params: serde_json::to_value(ExecutorAction {
                    action: ExecutorActionKind::Health,
                })
                .unwrap(),
                deadline_ms: Some(900_000),
                priority: Priority::Interactive,
            })
            .await;

        assert!(matches!(response, Response::Ok { .. }));
    }

    #[tokio::test]
    async fn sqlite_cannot_supply_runtime_argv_or_mounts() {
        let heartbeats = Arc::new(Heartbeats::default());
        heartbeats.mark_guard();
        heartbeats.mark_events();
        let handler = ExecutorHandler {
            agent_uid: 996,
            cancellation: Arc::new(CancelRegistry::new()),
            docker: Arc::new(BollardDockerInspector),
            runtime: Arc::new(BollardContainerRuntime),
            heartbeats,
            hostname_path: "/etc/hostname".into(),
            kernel_release: "6.17-test".into(),
            machine_id_path: "/etc/machine-id".into(),
            catalog: catalog(),
            engine_policy: engine_policy(),
            recipe_host: recipe_host(),
            resources: None,
        };
        let result = handler
            .execute(
                ExecutorAction {
                    action: ExecutorActionKind::InspectRecipes(RecipeQuery {
                        model_repository: Some("ornith-ai/Ornith-1.5-9B".into()),
                        model_commit: None,
                        objective: "agent".into(),
                    }),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("argv") && !serialized.contains("mounts"));
        assert!(serde_json::from_value::<ExecutorAction>(serde_json::json!({
            "action": {"inspect_recipes": {
                "model_repository": "ornith-ai/Ornith-1.5-9B",
                "model_commit": null,
                "objective": "agent",
                "argv": ["attacker"]
            }}
        }))
        .is_err());
    }

    #[test]
    fn legacy_spec_fixture_remains_locked_down_for_migration_checks() {
        let catalog = RecipeCatalog::signed_for_test();
        let recipe = catalog.recipe("ornith-1.5-9b-vllm-0.19.1").unwrap();
        let spec = build_container_spec(
            recipe,
            &StartInstanceInput {
                instance_id: format!("i_{}", "1".repeat(32)),
                generation: 3,
                model_commit: recipe.model.commits[0].clone(),
                model_repository: recipe.model.repository.clone(),
                recipe_id: recipe.identity.id.clone(),
                operation_id: "01K00000000000000000000000".into(),
            },
            "test-internal-network",
        )
        .unwrap();
        let value = serde_json::to_value(spec).unwrap();

        assert_eq!(
            value["image"],
            format!(
                "{}@{}",
                recipe.engine.image_repository, recipe.engine.image_digest
            )
        );
        assert_eq!(value["network"], "test-internal-network");
        assert_eq!(value["restart"], "no");
        assert_eq!(value["read_only_rootfs"], true);
        assert_eq!(value["cap_drop"], serde_json::json!(["ALL"]));
        assert_eq!(value["published_ports"], serde_json::json!([]));
        assert_eq!(value["accelerator"], "nvidia");
        assert_eq!(
            value["gateway_profile"],
            serde_json::to_value(recipe.gateway.profile()).unwrap()
        );
        assert_eq!(value["environment"], serde_json::json!([]));
        assert!(value["mounts"].as_array().unwrap().iter().any(|mount| {
            mount["source"]
                .as_str()
                .is_some_and(|source| source.contains("/sha256-") && !source.ends_with("/3"))
        }));
        assert!(value["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|mount| mount["source"]
                .as_str()
                .unwrap()
                .starts_with("/var/lib/sy-spark/")));
        assert!(!serde_json::to_string(&value)
            .unwrap()
            .contains("docker.sock"));
    }

    #[test]
    fn generic_spec_accepts_unlisted_repository_and_owns_security_fields() {
        let policy = engine_policy();
        let input = StartInstanceInput {
            instance_id: format!("i_{}", "2".repeat(32)),
            generation: 1,
            model_commit: "3".repeat(40),
            model_repository: "new-owner/model-never-listed-in-sy".into(),
            recipe_id: policy.config().id.clone(),
            operation_id: "01K00000000000000000000000".into(),
        };
        let spec = build_generic_container_spec(&policy, &input).unwrap();

        assert_eq!(spec.image, policy.image());
        assert_eq!(spec.entrypoint, policy.config().entrypoint);
        assert_eq!(spec.network, policy.config().network);
        assert_eq!(spec.run_as_uid, policy.config().run_as_uid);
        assert_eq!(spec.cap_drop, ["ALL"]);
        assert!(spec.published_ports.is_empty());
        assert!(spec.mounts[0]
            .source
            .ends_with("models--new-owner--model-never-listed-in-sy"));
        assert_eq!(spec.mounts[0].target, "/models");
        assert!(
            serde_json::from_value::<StartInstanceInput>(serde_json::json!({
                "instance_id": input.instance_id,
                "generation": input.generation,
                "model_commit": input.model_commit,
                "model_repository": input.model_repository,
                "recipe_id": input.recipe_id,
                "operation_id": input.operation_id,
                "image": "attacker/image",
                "argv": ["sh", "-c", "id"],
                "mounts": ["/:/host"]
            }))
            .is_err()
        );
    }

    #[test]
    fn attested_uid_change_cannot_reuse_a_stale_compile_cache_namespace() {
        let catalog = RecipeCatalog::signed_for_test();
        let recipe = catalog.recipe("ornith-1.5-9b-vllm-0.19.1").unwrap();
        let previous = recipe_compile_cache_key(recipe, &recipe.model.commits[0]);
        let mut changed = recipe.clone();
        changed.isolation.run_as_uid = 10_001;
        changed.evidence.image_run_as_uid = 10_001;
        assert_ne!(
            previous,
            recipe_compile_cache_key(&changed, &changed.model.commits[0])
        );
    }

    #[test]
    fn evidence_status_and_semantic_probe_changes_preserve_compile_cache_namespace() {
        let catalog = RecipeCatalog::signed_for_test();
        let recipe = catalog.recipe("ornith-1.5-9b-vllm-0.19.1").unwrap();
        let key = recipe_compile_cache_key(recipe, &recipe.model.commits[0]);
        let mut changed = recipe.clone();
        changed.evidence.quality = "final local verification attached".into();
        changed
            .evidence
            .measured_metrics
            .push("semantic-ready=true".into());
        changed.identity.status = super::super::wire::RecipeStatus::UpstreamVerified;
        changed.health.semantic_prompt = "A different one-token probe".into();
        changed.health.startup_deadline_seconds = 600;
        changed.gateway.port += 1;
        changed.resources.image_bytes += 1;
        changed.resources.startup_peak_bytes += 1;
        changed.provenance.redistribution = "updated publication terms".into();

        assert_eq!(
            key,
            recipe_compile_cache_key(&changed, &changed.model.commits[0])
        );
    }

    #[test]
    fn compile_affecting_identity_changes_isolate_cache_namespace() {
        let catalog = RecipeCatalog::signed_for_test();
        let recipe = catalog.recipe("ornith-1.5-9b-vllm-0.19.1").unwrap();
        let commit = &recipe.model.commits[0];
        let key = recipe_compile_cache_key(recipe, commit).unwrap();
        let changed_key = |changed: &super::super::recipe::Recipe, selected_commit: &str| {
            assert_ne!(
                key,
                recipe_compile_cache_key(changed, selected_commit).unwrap()
            );
        };

        let mut changed = recipe.clone();
        changed.resources.context_ceiling -= 1;
        changed_key(&changed, commit);
        let mut changed = recipe.clone();
        changed.engine.argv.push("--enforce-eager".into());
        changed_key(&changed, commit);
        let mut changed = recipe.clone();
        changed.engine.image_digest = format!("sha256:{}", "0".repeat(64));
        changed_key(&changed, commit);
        let mut changed = recipe.clone();
        changed.model.repository = "ornith-ai/changed".into();
        changed_key(&changed, commit);
        let mut changed = recipe.clone();
        changed.model.parser_sha256 = "0".repeat(64);
        changed_key(&changed, commit);
        let mut changed = recipe.clone();
        changed.isolation.run_as_uid += 1;
        changed_key(&changed, commit);
        let mut changed = recipe.clone();
        changed.evidence.image_run_as_uid += 1;
        changed_key(&changed, commit);
        changed_key(recipe, &"c".repeat(40));
    }

    #[test]
    fn docker_default_seccomp_is_selected_by_omitting_a_profile_override() {
        let spec = build_generic_container_spec(&engine_policy(), &generic_input('1')).unwrap();
        assert_eq!(security_options(&spec), ["no-new-privileges=true"]);
    }

    #[test]
    fn trusted_cache_group_is_added_without_chown_capability() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let model_root = root.path().join("models");
        let cache_root = root.path().join("compile-cache");
        std::fs::create_dir_all(model_root.join("repository")).unwrap();
        std::fs::create_dir(&cache_root).unwrap();
        std::fs::set_permissions(&model_root, std::fs::Permissions::from_mode(0o750)).unwrap();
        std::fs::set_permissions(&cache_root, std::fs::Permissions::from_mode(0o750)).unwrap();
        let owner = cache_root.metadata().unwrap().uid();
        let group = cache_root.metadata().unwrap().gid();
        let catalog = RecipeCatalog::signed_for_test();
        let recipe = catalog.recipe("ornith-1.5-9b-vllm-0.19.1").unwrap();
        let mut spec = build_container_spec(
            recipe,
            &StartInstanceInput {
                instance_id: format!("i_{}", "1".repeat(32)),
                generation: 3,
                model_commit: recipe.model.commits[0].clone(),
                model_repository: recipe.model.repository.clone(),
                recipe_id: recipe.identity.id.clone(),
                operation_id: "01K00000000000000000000000".into(),
            },
            "test-internal-network",
        )
        .unwrap();
        for mount in &mut spec.mounts {
            mount.source = if mount.read_only {
                model_root.join("repository")
            } else {
                cache_root.join("fingerprint/instance")
            }
            .to_string_lossy()
            .into_owned();
        }

        let groups = validate_mount_sources_at(&spec, &model_root, &cache_root, owner).unwrap();
        let cache = spec.mounts.iter().find(|mount| !mount.read_only).unwrap();
        let metadata = std::fs::metadata(&cache.source).unwrap();
        assert_eq!(groups, [group.to_string()]);
        assert_eq!((metadata.uid(), metadata.gid()), (owner, group));
        assert_eq!(metadata.permissions().mode() & 0o777, 0o770);
    }

    #[test]
    fn docker_create_failure_diagnostic_is_bounded_and_drops_raw_details() {
        let error = bollard::errors::Error::DockerResponseServerError {
            status_code: 400,
            message: "unknown or invalid runtime name: nvidia /secret/host/path".into(),
        };
        let diagnostic = docker_failure_diagnostic("create", &error);

        assert_eq!(
            diagnostic,
            "stage=create cause=nvidia-runtime-unavailable status=400"
        );
        assert!(!diagnostic.contains("secret") && diagnostic.len() < 128);
    }

    #[test]
    fn writable_tmpfs_is_owned_by_the_attested_nonroot_uid() {
        assert_eq!(
            tmpfs_options(65_534, "992"),
            "rw,noexec,nosuid,nodev,size=1073741824,mode=1770,uid=65534,gid=992"
        );
    }

    #[test]
    fn image_exposed_port_without_host_binding_is_not_published() {
        let exposed = std::collections::HashMap::from([("5678/tcp".into(), None)]);
        let published = std::collections::HashMap::from([(
            "8000/tcp".into(),
            Some(vec![bollard::models::PortBinding {
                host_ip: Some("0.0.0.0".into()),
                host_port: Some("8000".into()),
            }]),
        )]);
        assert!(!has_published_ports(&exposed));
        assert!(has_published_ports(&published));
    }

    #[test]
    fn local_image_requires_the_exact_repository_digest_and_architecture() {
        const IMAGE: &str = "vllm/vllm-openai@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let exact = bollard::models::ImageInspect {
            repo_digests: Some(vec![IMAGE.into()]),
            architecture: Some("arm64".into()),
            ..Default::default()
        };
        assert!(exact_image_matches(IMAGE, "arm64", &exact));

        let wrong_arch = bollard::models::ImageInspect {
            architecture: Some("amd64".into()),
            ..exact.clone()
        };
        assert!(!exact_image_matches(IMAGE, "arm64", &wrong_arch));
        let tag_only = bollard::models::ImageInspect {
            repo_digests: None,
            repo_tags: Some(vec!["vllm/vllm-openai:latest".into()]),
            ..exact
        };
        assert!(!exact_image_matches(IMAGE, "arm64", &tag_only));
        assert!(!exact_image_matches(
            "vllm/vllm-openai:latest",
            "arm64",
            &tag_only
        ));
    }

    #[tokio::test]
    async fn prepare_phase_pulls_before_create_and_second_stop_succeeds() {
        let runtime = FakeRuntime::new();
        let calls = runtime.calls.clone();
        let handler = ExecutorHandler {
            agent_uid: 996,
            cancellation: Arc::new(CancelRegistry::new()),
            docker: Arc::new(BollardDockerInspector),
            runtime: Arc::new(runtime),
            heartbeats: Arc::new(Heartbeats::default()),
            hostname_path: "/etc/hostname".into(),
            kernel_release: "6.17-test".into(),
            machine_id_path: "/etc/machine-id".into(),
            catalog: catalog(),
            engine_policy: engine_policy(),
            recipe_host: recipe_host(),
            resources: None,
        };
        let input = generic_input('1');
        let prepared = handler
            .execute(
                ExecutorAction {
                    action: ExecutorActionKind::PrepareInstance(input.clone()),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            prepared,
            ExecutorResult::PrepareInstance {
                startup_deadline_seconds: 900
            }
        ));
        handler
            .execute(
                ExecutorAction {
                    action: ExecutorActionKind::StartInstance(input),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let stop = StopInstanceInput {
            instance_id: format!("i_{}", "1".repeat(32)),
            generation: 1,
            grace_seconds: 1,
        };
        for _ in 0..2 {
            handler
                .execute(
                    ExecutorAction {
                        action: ExecutorActionKind::StopInstance(stop.clone()),
                    },
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0], "network");
        assert!(calls[1].starts_with("image:vllm/vllm-openai@sha256:"));
        assert_eq!(calls[2], "start");
        assert_eq!(
            calls.iter().filter(|call| call.as_str() == "stop").count(),
            2
        );
    }

    #[tokio::test]
    async fn legacy_recipe_identity_cannot_start_a_new_generation() {
        let runtime = FakeRuntime::new();
        let calls = runtime.calls.clone();
        let handler = ExecutorHandler {
            agent_uid: 996,
            cancellation: Arc::new(CancelRegistry::new()),
            docker: Arc::new(BollardDockerInspector),
            runtime: Arc::new(runtime),
            heartbeats: Arc::new(Heartbeats::default()),
            hostname_path: "/etc/hostname".into(),
            kernel_release: "6.17-test".into(),
            machine_id_path: "/etc/machine-id".into(),
            catalog: catalog(),
            engine_policy: engine_policy(),
            recipe_host: recipe_host(),
            resources: None,
        };
        let recipe = RecipeCatalog::signed_for_test()
            .recipe("spark-fixture-http-echo-1.0.0")
            .unwrap()
            .clone();
        let input = StartInstanceInput {
            instance_id: format!("i_{}", "1".repeat(32)),
            generation: 1,
            model_commit: recipe.model.commits[0].clone(),
            model_repository: recipe.model.repository,
            recipe_id: recipe.identity.id,
            operation_id: "01K00000000000000000000000".into(),
        };
        for action in [
            ExecutorActionKind::PrepareInstance(input.clone()),
            ExecutorActionKind::StartInstance(input.clone()),
        ] {
            assert!(matches!(
                handler
                    .execute(ExecutorAction { action }, CancellationToken::new())
                    .await,
                Err(ErrorCode::BadRequest)
            ));
        }
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn full_scan_matches_only_exact_catalogued_generation_and_quarantines_name_adoption() {
        let recipes = RecipeCatalog::signed_for_test();
        let recipe = recipes.recipe("spark-fixture-http-echo-1.0.0").unwrap();
        let instance_id = format!("i_{}", "1".repeat(32));
        let expected = ReconcileExpectation {
            instance_id: instance_id.clone(),
            generation: 1,
            model_commit: recipe.model.commits[0].clone(),
            model_repository: recipe.model.repository.clone(),
            recipe_id: recipe.identity.id.clone(),
        };
        let runtime = FakeRuntime::new();
        let managed = runtime.managed.clone();
        managed.lock().unwrap().push(ManagedContainerObservation {
            container_id: "a".repeat(64),
            name: "operator-picked-name".into(),
            instance_id: Some(instance_id),
            generation: Some(1),
            role: Some("engine".into()),
            model_commit: Some(expected.model_commit.clone()),
            model_repository: Some(expected.model_repository.clone()),
            recipe_id: Some(expected.recipe_id.clone()),
            image: Some(format!(
                "{}@{}",
                recipe.engine.image_repository, recipe.engine.image_digest
            )),
            networks: vec!["legacy-internal-network".into()],
            restart_policy: "unless-stopped".into(),
        });
        let handler = ExecutorHandler {
            agent_uid: 996,
            cancellation: Arc::new(CancelRegistry::new()),
            docker: Arc::new(BollardDockerInspector),
            runtime: Arc::new(runtime),
            heartbeats: Arc::new(Heartbeats::default()),
            hostname_path: "/etc/hostname".into(),
            kernel_release: "6.17-test".into(),
            machine_id_path: "/etc/machine-id".into(),
            catalog: catalog(),
            engine_policy: engine_policy(),
            recipe_host: recipe_host(),
            resources: None,
        };
        let result = handler
            .execute(
                ExecutorAction {
                    action: ExecutorActionKind::ReconcileScan(vec![expected.clone()]),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ExecutorResult::ReconcileScan { scan } = result else {
            panic!("wrong result");
        };
        assert_eq!(
            (
                scan.matched.len(),
                scan.missing.len(),
                scan.quarantined.len()
            ),
            (0, 1, 1)
        );
        managed.lock().unwrap()[0].name =
            format!("sy-spark-{}-g{}", expected.instance_id, expected.generation);
        let result = handler
            .execute(
                ExecutorAction {
                    action: ExecutorActionKind::ReconcileScan(vec![expected]),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ExecutorResult::ReconcileScan { scan } = result else {
            panic!("wrong result");
        };
        assert_eq!((scan.matched.len(), scan.missing.len()), (1, 0));
    }

    #[tokio::test]
    async fn health_failure_disables_restart_and_compensates_generation() {
        let runtime = FakeRuntime::new();
        let stops = runtime.stops.clone();
        let monitor = Arc::new(ResourceMonitor::new(
            Box::new(StaticSampler(Err(
                super::super::resources::SampleError::Unavailable,
            ))),
            ResourcePolicy::capacity_first(),
            Arc::new(Heartbeats::default()),
            tempfile::tempdir().unwrap().path().join("emergency.jsonl"),
        ));
        let handler = ExecutorHandler {
            agent_uid: 996,
            cancellation: Arc::new(CancelRegistry::new()),
            docker: Arc::new(BollardDockerInspector),
            runtime: Arc::new(runtime),
            heartbeats: Arc::new(Heartbeats::default()),
            hostname_path: "/etc/hostname".into(),
            kernel_release: "6.17-test".into(),
            machine_id_path: "/etc/machine-id".into(),
            catalog: catalog(),
            engine_policy: engine_policy(),
            recipe_host: recipe_host(),
            resources: Some(monitor.clone()),
        };
        let input = generic_input('1');
        handler
            .execute(
                ExecutorAction {
                    action: ExecutorActionKind::StartInstance(input),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let compensation = StopInstanceInput {
            instance_id: format!("i_{}", "1".repeat(32)),
            generation: 1,
            grace_seconds: 5,
        };
        handler
            .execute(
                ExecutorAction {
                    action: ExecutorActionKind::StopInstance(compensation.clone()),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(stops.lock().unwrap().as_slice(), [compensation]);
        assert!(monitor.managed.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn emergency_disables_restart_then_kills_exact_observed_cgroup() {
        let root = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::new();
        let calls = runtime.calls.clone();
        let observed = observed_engine();
        let cgroup = root.path().join(&observed.cgroup_path);
        std::fs::create_dir_all(&cgroup).unwrap();
        std::fs::write(cgroup.join("cgroup.kill"), b"").unwrap();
        let monitor = ResourceMonitor::new(
            Box::new(StaticSampler(Err(
                super::super::resources::SampleError::Unavailable,
            ))),
            ResourcePolicy::capacity_first(),
            Arc::new(Heartbeats::default()),
            root.path().join("emergency.jsonl"),
        );
        monitor.observe_engine(&observed, super::super::resources::EnginePhase::Starting);
        let decision = super::super::resources::EmergencyDecision {
            schema: "sy.spark.emergency-decision/v1".into(),
            instance_id: observed.instance_id.clone(),
            generation: observed.generation,
            cause: "memory-available-floor".into(),
            mem_available_bytes: 1,
            memory_full_psi_avg10_percent: 0.0,
        };

        assert!(actuate_emergency_at(&monitor, &runtime, &decision, root.path()).await);
        assert_eq!(std::fs::read(cgroup.join("cgroup.kill")).unwrap(), b"1");
        assert_eq!(calls.lock().unwrap().as_slice(), ["disable"]);
    }

    #[test]
    fn engine_logs_redact_complete_credential_lines() {
        assert_eq!(
            redact_log_line("Authorization: Bearer secret"),
            "[REDACTED]"
        );
        assert_eq!(redact_log_line("token=secret"), "[REDACTED]");
        assert_eq!(redact_log_line("ready on port 8000"), "ready on port 8000");
        let (cursor, line) =
            timestamped_log_line("2026-08-24T12:00:00.000000123Z Authorization: Bearer secret")
                .unwrap();
        assert!(cursor > 1_000_000_000);
        assert_eq!(line, "[REDACTED]");
    }

    #[test]
    fn stop_target_requires_exact_managed_identity() {
        let input = StopInstanceInput {
            instance_id: format!("i_{}", "1".repeat(32)),
            generation: 7,
            grace_seconds: 5,
        };
        let mut labels = std::collections::HashMap::from([
            ("io.sy.spark.managed".into(), "true".into()),
            ("io.sy.spark.role".into(), "engine".into()),
            ("io.sy.spark.instance".into(), input.instance_id.clone()),
            ("io.sy.spark.generation".into(), "7".into()),
        ]);

        assert!(exact_managed_identity(&labels, &input));
        labels.insert("io.sy.spark.generation".into(), "6".into());
        assert!(!exact_managed_identity(&labels, &input));
    }

    #[test]
    fn managed_observation_tracks_exact_cgroup_memory_growth() {
        let root = tempfile::tempdir().unwrap();
        let observed = observed_engine();
        let cgroup = root.path().join(&observed.cgroup_path);
        std::fs::create_dir_all(&cgroup).unwrap();
        std::fs::write(cgroup.join("memory.current"), "10\n").unwrap();
        let monitor = ResourceMonitor::new(
            Box::new(StaticSampler(Err(
                super::super::resources::SampleError::Unavailable,
            ))),
            ResourcePolicy::capacity_first(),
            Arc::new(Heartbeats::default()),
            root.path().join("emergency.jsonl"),
        );

        monitor.observe_engine(&observed, super::super::resources::EnginePhase::Healthy);
        monitor.refresh_managed_memory_at(root.path());
        std::fs::write(cgroup.join("memory.current"), "20\n").unwrap();
        monitor.refresh_managed_memory_at(root.path());

        let managed = monitor.managed.read().unwrap();
        assert_eq!(
            (managed[0].previous_memory_bytes, managed[0].memory_bytes),
            (10, 20)
        );
    }
}
