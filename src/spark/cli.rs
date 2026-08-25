//! CLIG surface for workstation bootstrap planning.

use std::{env, fmt, path::PathBuf, time::Duration};

use clap::{Args, Subcommand};
use serde::Serialize;

use super::{
    client::{self, SparkClient},
    wire::{
        BenchRequest, CertificateStatusDocument, CompatibilityEvaluationDocument, DoctorDocument,
        DownloadRequest, ModelDocument, ModelListDocument, OperationDocument,
        OperationListDocument, RecipeCatalogDocument, RemoveModelRequest, ServeAdmissionRequest,
        ServeRequest, StatusDocument, StopRequest, TokenCreateRequest, TokenCreatedDocument,
        TokenListDocument, TuneRequest,
    },
    EXIT_USAGE,
};
use super::{
    install::{self, InstallErrorKind, InstallRequest, OpenSshRunner},
    EXIT_UNREACHABLE,
};

const DEFAULT_PROBE: &str = "/usr/libexec/sy/spark-bootstrap-aarch64";
const DEFAULT_LISTEN_PORT: u16 = 9843;

#[derive(Debug, Args)]
#[command(
    about = "Inspect and manage one configured DGX Spark appliance",
    long_about = "Inspect and install one configured DGX Spark appliance, then use its authenticated read-only HTTPS status plane. The host is passed to OpenSSH as one argument; there is no arbitrary-command escape hatch."
)]
pub struct SparkCli {
    /// Existing OpenSSH alias, such as dgx-spark.
    pub host: String,
    #[command(subcommand)]
    pub command: SparkCommand,
}

#[derive(Debug, Subcommand)]
pub enum SparkCommand {
    /// Inspect the appliance and print the exact non-mutating installation plan.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark install --dry-run --json\n  sy spark dgx-spark install --yes --release-signature sy-aarch64.minisig --release-public-key sy-release.pub\n\nEnvironment:\n  SY_SPARK_DRY_RUN, SY_SPARK_YES, SY_SPARK_JSON, SY_SPARK_PROBE, SY_SPARK_LISTEN_ADDRESS, SY_SPARK_LISTEN_PORT, SY_SPARK_RELEASE_SIGNATURE, SY_SPARK_RELEASE_PUBLIC_KEY, SY_SPARK_CONFIG_DIR\n  Flags override environment values, which override declarative defaults.\n\nAuthentication:\n  OpenSSH owns known_hosts, agents, hardware tokens, keyboard-interactive and interactive password prompts. Credentials are never accepted as sy arguments or stored by sy.\n\nExit codes:\n  0 success; 2 usage/local configuration; 4 OpenSSH/SFTP/agent unreachable, TLS identity mismatch, or authentication failure.\n\nSecurity:\n  Dry-run uploads one content-addressed probe, invokes only `spark bootstrap inspect`, verifies its hash, and removes that exact path. Approved install uploads only signed content-addressed release inputs and invokes the fixed bootstrap activation entrypoint. No arbitrary remote command is accepted."
    )]
    Install(InstallArgs),
    /// Upgrade the signed control plane side by side without stopping engines.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark upgrade --dry-run --json\n  sy spark dgx-spark upgrade --yes --release-signature sy-aarch64.minisig --release-public-key sy-release.pub --json\n\nEnvironment:\n  SY_SPARK_DRY_RUN, SY_SPARK_YES, SY_SPARK_JSON, SY_SPARK_PROBE, SY_SPARK_RELEASE_SIGNATURE, SY_SPARK_RELEASE_PUBLIC_KEY, SY_SPARK_CONFIG_DIR\n\nExit codes:\n  0 success; 2 usage/local configuration; 3 compatibility or safety rejection; 4 SSH, TLS, or agent unreachable."
    )]
    Upgrade(InstallArgs),
    /// Atomically return to the verified preceding control-plane release.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark rollback --dry-run --json\n  sy spark dgx-spark rollback --yes --json\n\nHealthy engine containers are preserved. Docker restart and host reboot are never performed.\n\nEnvironment:\n  SY_SPARK_DRY_RUN, SY_SPARK_YES, SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    Rollback(MaintenanceArgs),
    /// Show compact authenticated agent/executor health over pinned HTTPS.
    #[command(
        after_help = "Example:\n  sy spark dgx-spark status --json\n\nExit codes:\n  0 success; 1 unexpected failure; 2 local configuration; 3 remote policy/state rejection; 4 unreachable, TLS pin mismatch, or authentication failure."
    )]
    Status(ReadArgs),
    /// Run authenticated, read-only compatibility and security checks.
    #[command(
        after_help = "Example:\n  sy spark dgx-spark doctor --json\n\nExit codes:\n  0 success; 1 unexpected failure; 2 local configuration; 3 remote policy/state rejection; 4 unreachable, TLS pin mismatch, or authentication failure."
    )]
    Doctor(ReadArgs),
    /// Inspect, follow, or cancel durable operations.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark operations --json\n  sy spark dgx-spark operations 01K... --follow --json\n  sy spark dgx-spark operations cancel 01K... --dry-run\n\nEnvironment:\n  SY_SPARK_JSON, SY_SPARK_CONFIG_DIR, SY_SPARK_IDEMPOTENCY_KEY, SY_SPARK_DRY_RUN"
    )]
    Operations(OperationsArgs),
    /// Create, list, or revoke scoped bearer tokens.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark token create --name reader --scope models:read --scope operations:read --detach --json\n  sy spark dgx-spark token list --json\n  sy spark dgx-spark token revoke 01K... --yes --json\n\nSecrets are returned once by create and are never written by sy outside stdout.\n\nEnvironment:\n  SY_SPARK_JSON, SY_SPARK_CONFIG_DIR, SY_SPARK_IDEMPOTENCY_KEY, SY_SPARK_DRY_RUN, SY_SPARK_YES"
    )]
    Token(TokenArgs),
    /// Explain every signed engine recipe and preview deterministic selection.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark recipes --json\n  sy spark dgx-spark recipes ornith-ai/Ornith-1.5-9B --json\n\nThis command is read-only: it never downloads a model, pulls an image, or starts an engine."
    )]
    Recipes(RecipesArgs),
    /// Evaluate one exact installed recipe against bounded functional gates.
    #[command(
        after_help = "Example:\n  sy spark dgx-spark bench ornith-1.5:9b --json\n\nBench does not measure speed, download software, pull images, or start an engine. It evaluates exact compatibility, capability, correctness evidence, safety, isolation, and durability."
    )]
    Bench(BenchArgs),
    /// Select from installed locally verified recipes using functional evidence.
    #[command(
        after_help = "Example:\n  sy spark dgx-spark tune ornith-1.5:9b --objective agent --json\n\nTune persists a deterministic compatible winner. Unsupported engine families remain explicit and are never downloaded implicitly; verified vLLM remains the visible fallback."
    )]
    Tune(TuneArgs),
    /// Acquire and verify one immutable Hugging Face model snapshot.
    #[command(
        after_help = "Example:\n  sy spark dgx-spark download ornith-ai/Ornith-1.5-9B --alias ornith-1.5:9b --detach --json\n\nEnvironment:\n  SY_SPARK_REVISION, SY_SPARK_ALIAS, SY_SPARK_UPDATE_ALIAS, SY_SPARK_DETACH, SY_SPARK_DRY_RUN, SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    Download(DownloadArgs),
    /// Start one recipe-selected managed model instance after resource admission.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark serve ornith-1.5:9b\n  sy spark dgx-spark serve ornith-1.5:9b --dry-run --json\n\nServe starts a digest-pinned, isolated engine after fail-closed admission. The dry-run performs selection and admission without Docker or GPU side effects.\n\nExit codes:\n  0 success; 1 unexpected failure; 2 usage/local configuration; 3 remote policy/state rejection; 4 unreachable, TLS pin mismatch, or authentication failure."
    )]
    Serve(ServeArgs),
    /// List desired and observed managed model instances.
    Ps(ReadArgs),
    /// Read bounded redacted logs for one managed instance.
    Logs(LogsArgs),
    /// Persist stopped intent, drain, and remove one managed instance.
    Stop(StopArgs),
    /// List complete verified local model snapshots.
    Ls(ReadArgs),
    /// Show immutable identity, provenance, aliases, and references for one model.
    Show(ModelReadArgs),
    /// Preview or remove only unreferenced native-cache model data.
    Rm(RemoveArgs),
    /// Render a user-level client configuration without reading or writing its token.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark client-config ornith --client codex\n  sy spark dgx-spark client-config ornith --client claude-code\n\nOutput is user-level, names the required secret environment variable without reading or writing it, and prints pinned-CA guidance. Codex disables web search and WebSockets; Claude Code disables nonessential traffic.\n\nEnvironment:\n  SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    ClientConfig(ClientConfigArgs),
    /// Inspect the authenticated certificate identity.
    Cert(CertArgs),
    #[cfg(feature = "spark-agent")]
    #[command(hide = true)]
    RunAgent(AgentArgs),
    #[cfg(feature = "spark-agent")]
    #[command(hide = true)]
    RunExecutor(ExecutorArgs),
    #[cfg(feature = "spark-agent")]
    #[command(hide = true)]
    Activate(ActivateArgs),
    #[command(hide = true)]
    Inspect,
}

#[derive(Debug, Args)]
pub struct OperationsArgs {
    /// Operation ID to inspect.
    pub id: Option<String>,
    /// Resume events and poll until the operation reaches terminal state.
    #[arg(long)]
    pub follow: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<OperationsCommand>,
}

#[derive(Debug, Subcommand)]
pub enum OperationsCommand {
    /// Request idempotent cancellation of a non-terminal operation.
    Cancel(OperationCancelArgs),
}

#[derive(Debug, Args)]
pub struct OperationCancelArgs {
    pub id: String,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_IDEMPOTENCY_KEY")]
    pub idempotency_key: Option<String>,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct TokenArgs {
    #[command(subcommand)]
    pub command: TokenCommand,
}

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Create a least-privilege token; bearer material is returned once.
    Create(TokenCreateArgs),
    /// List redacted token metadata.
    List(ReadArgs),
    /// Revoke a token before returning success.
    Revoke(TokenRevokeArgs),
}

#[derive(Debug, Args)]
pub struct TokenCreateArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long = "scope", required = true)]
    pub scopes: Vec<super::wire::TokenScope>,
    #[arg(long = "cidr")]
    pub allowed_cidrs: Vec<String>,
    #[arg(long)]
    pub expires_at: Option<String>,
    #[arg(long, default_value_t = 1)]
    pub max_concurrent_inference: u32,
    #[arg(long)]
    pub detach: bool,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_IDEMPOTENCY_KEY")]
    pub idempotency_key: Option<String>,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct TokenRevokeArgs {
    pub id: String,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_YES")]
    pub yes: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_IDEMPOTENCY_KEY")]
    pub idempotency_key: Option<String>,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CertArgs {
    #[command(subcommand)]
    pub command: CertCommand,
}

#[derive(Debug, Subcommand)]
pub enum CertCommand {
    /// Show the authenticated leaf-certificate status.
    #[command(
        override_usage = "sy spark <HOST> cert status [OPTIONS]",
        after_help = "Example:\n  sy spark dgx-spark cert status --json\n\nExit codes:\n  0 success; 1 unexpected failure; 2 local configuration; 3 remote policy/state rejection; 4 unreachable, TLS pin mismatch, or authentication failure."
    )]
    Status(ReadArgs),
    /// Rotate the leaf certificate over SSH with an overlap copy.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark cert rotate --dry-run --json\n  sy spark dgx-spark cert rotate --yes --json\n  sy spark dgx-spark cert rotate --ca --yes --json\n\nCA rotation updates the local pin only after SSH returns the new public CA. Private keys never leave Spark.\n\nEnvironment:\n  SY_SPARK_DRY_RUN, SY_SPARK_YES, SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    Rotate(CertificateRotateArgs),
}

#[derive(Debug, Args)]
pub struct MaintenanceArgs {
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_YES")]
    pub yes: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CertificateRotateArgs {
    /// Rotate the local CA as well as the leaf and require client re-pinning.
    #[arg(long)]
    pub ca: bool,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_YES")]
    pub yes: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ReadArgs {
    /// Emit the command's stable versioned JSON document.
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    /// Directory containing spark.toml, pinned CA certificates, and credentials.
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ClientConfigArgs {
    pub instance: String,
    #[arg(long, value_enum)]
    pub client: ClientKind,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ClientKind {
    Codex,
    ClaudeCode,
}

#[derive(Debug, Args)]
pub struct RecipesArgs {
    /// Exact model repository whose compatibility should be explained.
    pub model: Option<String>,
    /// Emit the stable sy.spark.recipe-catalog/v1 document.
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    pub model: String,
    #[arg(long, env = "SY_SPARK_RECIPE")]
    pub recipe: Option<String>,
    #[arg(long, default_value = "agent", env = "SY_SPARK_OBJECTIVE")]
    pub objective: String,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_IDEMPOTENCY_KEY")]
    pub idempotency_key: Option<String>,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct TuneArgs {
    pub model: String,
    #[arg(long, default_value = "agent", env = "SY_SPARK_OBJECTIVE")]
    pub objective: String,
    #[arg(long, env = "SY_SPARK_DETACH")]
    pub detach: bool,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_IDEMPOTENCY_KEY")]
    pub idempotency_key: Option<String>,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DownloadArgs {
    pub repository: String,
    #[arg(long, default_value = "main", env = "SY_SPARK_REVISION")]
    pub revision: String,
    #[arg(long, env = "SY_SPARK_ALIAS")]
    pub alias: Option<String>,
    #[arg(long, env = "SY_SPARK_UPDATE_ALIAS")]
    pub update_alias: bool,
    #[arg(long, env = "SY_SPARK_DETACH")]
    pub detach: bool,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_IDEMPOTENCY_KEY")]
    pub idempotency_key: Option<String>,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    pub model: String,
    #[arg(long, env = "SY_SPARK_INSTANCE_NAME")]
    pub name: Option<String>,
    #[arg(long, env = "SY_SPARK_RECIPE")]
    pub recipe: Option<String>,
    #[arg(long, default_value = "agent", env = "SY_SPARK_OBJECTIVE")]
    pub objective: String,
    #[arg(long, env = "SY_SPARK_ALLOW_UNVERIFIED")]
    pub allow_unverified: bool,
    #[arg(long, env = "SY_SPARK_DETACH")]
    pub detach: bool,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_IDEMPOTENCY_KEY")]
    pub idempotency_key: Option<String>,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    pub instance: String,
    #[arg(long, default_value_t = 30, env = "SY_SPARK_STOP_TIMEOUT_SECONDS")]
    pub timeout_seconds: u64,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_IDEMPOTENCY_KEY")]
    pub idempotency_key: Option<String>,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    pub instance: String,
    #[arg(long, env = "SY_SPARK_LOG_CURSOR", default_value_t = 0)]
    pub cursor: u64,
    #[arg(long, env = "SY_SPARK_LOG_LIMIT", default_value_t = 100)]
    pub limit: usize,
    #[arg(long, env = "SY_SPARK_FOLLOW")]
    pub follow: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ModelReadArgs {
    pub model: String,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    pub model: String,
    #[arg(long, env = "SY_SPARK_YES")]
    pub yes: bool,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_IDEMPOTENCY_KEY")]
    pub idempotency_key: Option<String>,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Args)]
pub struct AgentArgs {
    #[arg(long, default_value = "/etc/sy/spark-agent.toml")]
    pub config: PathBuf,
    #[arg(long, default_value = "/var/lib/sy-spark/tls/server-chain.pem")]
    pub certificate: PathBuf,
    #[arg(long, default_value = "/var/lib/sy-spark/tls/server-key.pem")]
    pub key: PathBuf,
    #[arg(
        long,
        default_value = "/run/credentials/sy-spark-agent.service/bootstrap-token"
    )]
    pub token: PathBuf,
    #[arg(
        long,
        default_value = "/run/credentials/sy-spark-agent.service/hf-token"
    )]
    pub hf_token: PathBuf,
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Args)]
pub struct ExecutorArgs {
    #[arg(long, default_value = "/etc/sy/spark-executor.toml")]
    pub config: PathBuf,
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Args)]
pub struct ActivateArgs {
    #[arg(long)]
    pub executable: PathBuf,
    #[arg(long)]
    pub signature: PathBuf,
    #[arg(long)]
    pub public_key: PathBuf,
    #[arg(long)]
    pub manifest: PathBuf,
    #[arg(long)]
    pub manifest_sha256: String,
    #[arg(long)]
    pub version: String,
    #[arg(long)]
    pub listen_address: String,
    #[arg(long)]
    pub hostname: String,
    #[arg(long)]
    pub active_lsm: String,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Inspect and render without installing.
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    /// Approve the reviewed manifest and perform the fixed installation transaction.
    #[arg(long, env = "SY_SPARK_YES")]
    pub yes: bool,
    /// Emit the stable sy.spark.install-manifest/v1 document on stdout.
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    /// ARM64 feature-minimal sy probe artifact.
    #[arg(long, value_name = "PATH", env = "SY_SPARK_PROBE")]
    pub probe: Option<PathBuf>,
    /// Explicit LAN address for the HTTPS listener.
    #[arg(long, value_name = "IP", env = "SY_SPARK_LISTEN_ADDRESS")]
    pub listen_address: Option<String>,
    /// HTTPS listener port (default 9843).
    #[arg(long, value_name = "PORT", env = "SY_SPARK_LISTEN_PORT")]
    pub listen_port: Option<u16>,
    /// Minisign signature for the ARM64 release (required with --yes).
    #[arg(long, value_name = "PATH", env = "SY_SPARK_RELEASE_SIGNATURE")]
    pub release_signature: Option<PathBuf>,
    /// Pinned minisign public key for the ARM64 release (required with --yes).
    #[arg(long, value_name = "PATH", env = "SY_SPARK_RELEASE_PUBLIC_KEY")]
    pub release_public_key: Option<PathBuf>,
    /// Protected local Spark configuration root.
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
}

impl InstallArgs {
    #[cfg(test)]
    fn dry_run_for_test() -> Self {
        Self {
            dry_run: true,
            yes: false,
            json: true,
            probe: None,
            listen_address: None,
            listen_port: None,
            release_signature: None,
            release_public_key: None,
            config_dir: None,
        }
    }
}

trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        env::var(key).ok()
    }
}

#[cfg(test)]
#[derive(Default)]
struct MapEnv(std::collections::BTreeMap<String, String>);

#[cfg(test)]
impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ResolvedInstall {
    dry_run: bool,
    yes: bool,
    json: bool,
    probe: PathBuf,
    listen_address: Option<String>,
    listen_port: u16,
    release_signature: Option<PathBuf>,
    release_public_key: Option<PathBuf>,
    config_dir: PathBuf,
}

fn resolve_install(args: &InstallArgs, env: &dyn EnvSource) -> Result<ResolvedInstall, SparkError> {
    let dry_run = args.dry_run || env_bool(env, "SY_SPARK_DRY_RUN")?.unwrap_or(false);
    let yes = args.yes || env_bool(env, "SY_SPARK_YES")?.unwrap_or(false);
    if !dry_run && !yes {
        return Err(SparkError::usage(
            "install requires either --dry-run or explicit --yes approval",
        ));
    }
    if dry_run && yes {
        return Err(SparkError::usage(
            "--dry-run and --yes are mutually exclusive",
        ));
    }
    let json = args.json || env_bool(env, "SY_SPARK_JSON")?.unwrap_or(false);
    let probe = args
        .probe
        .clone()
        .or_else(|| env.get("SY_SPARK_PROBE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_PROBE));
    let listen_address = args
        .listen_address
        .clone()
        .or_else(|| env.get("SY_SPARK_LISTEN_ADDRESS"));
    let listen_port = match args
        .listen_port
        .map(|port| port.to_string())
        .or_else(|| env.get("SY_SPARK_LISTEN_PORT"))
    {
        Some(value) => value.parse::<u16>().map_err(|_| {
            SparkError::usage(format!("SY_SPARK_LISTEN_PORT is not a valid port: {value}"))
        })?,
        None => DEFAULT_LISTEN_PORT,
    };
    Ok(ResolvedInstall {
        dry_run,
        yes,
        json,
        probe,
        listen_address,
        listen_port,
        release_signature: args.release_signature.clone(),
        release_public_key: args.release_public_key.clone(),
        config_dir: args.config_dir.clone().unwrap_or_else(default_config_dir),
    })
}

fn env_bool(env: &dyn EnvSource, key: &str) -> Result<Option<bool>, SparkError> {
    env.get(key)
        .map(|value| match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Ok(true),
            "0" | "false" | "no" => Ok(false),
            _ => Err(SparkError::usage(format!(
                "{key} must be true/false, yes/no, or 1/0"
            ))),
        })
        .transpose()
}

fn render_json(value: &impl Serialize) -> Result<String, SparkError> {
    serde_json::to_string_pretty(value)
        .map(|json| format!("{json}\n"))
        .map_err(|error| SparkError::usage(format!("encode Spark JSON: {error}")))
}

pub fn dispatch(cli: SparkCli) -> anyhow::Result<()> {
    match cli.command {
        SparkCommand::Install(args) => dispatch_install(cli.host, args, "install"),
        SparkCommand::Upgrade(args) => dispatch_install(cli.host, args, "upgrade"),
        SparkCommand::Rollback(args) => dispatch_rollback(cli.host, args),
        SparkCommand::Status(args) => dispatch_read::<StatusDocument>(
            &cli.host,
            args,
            "api/sy.spark/v1/status",
            render_status,
        ),
        SparkCommand::Doctor(args) => dispatch_read::<DoctorDocument>(
            &cli.host,
            args,
            "api/sy.spark/v1/doctor",
            render_doctor,
        ),
        SparkCommand::Operations(args) => dispatch_operations(&cli.host, args),
        SparkCommand::Token(args) => dispatch_tokens(&cli.host, args),
        SparkCommand::Recipes(args) => dispatch_recipes(&cli.host, args),
        SparkCommand::Bench(args) => dispatch_bench(&cli.host, args),
        SparkCommand::Tune(args) => dispatch_tune(&cli.host, args),
        SparkCommand::Download(args) => dispatch_download(&cli.host, args),
        SparkCommand::Serve(args) => dispatch_serve(&cli.host, args),
        SparkCommand::Ps(args) => dispatch_instances(&cli.host, args),
        SparkCommand::Logs(args) => dispatch_logs(&cli.host, args),
        SparkCommand::Stop(args) => dispatch_stop(&cli.host, args),
        SparkCommand::Ls(args) => dispatch_models(&cli.host, args),
        SparkCommand::Show(args) => dispatch_show(&cli.host, args),
        SparkCommand::Rm(args) => dispatch_remove(&cli.host, args),
        SparkCommand::ClientConfig(args) => dispatch_client_config(&cli.host, args),
        SparkCommand::Cert(CertArgs {
            command: CertCommand::Status(args),
        }) => dispatch_read::<CertificateStatusDocument>(
            &cli.host,
            args,
            "api/sy.spark/v1/certificates/status",
            render_certificate,
        ),
        SparkCommand::Cert(CertArgs {
            command: CertCommand::Rotate(args),
        }) => dispatch_certificate_rotate(cli.host, args),
        #[cfg(feature = "spark-agent")]
        SparkCommand::RunAgent(args) => dispatch_agent(cli.host, args),
        #[cfg(feature = "spark-agent")]
        SparkCommand::RunExecutor(args) => dispatch_executor(cli.host, args),
        #[cfg(feature = "spark-agent")]
        SparkCommand::Activate(args) => dispatch_activate(cli.host, args),
        SparkCommand::Inspect => dispatch_bootstrap(cli.host),
    }
    .map_err(Into::into)
}

fn dispatch_recipes(host: &str, args: RecipesArgs) -> Result<(), SparkError> {
    let client = load_client(host, args.config_dir)?;
    let document = client
        .recipes(args.model.as_deref())
        .map_err(SparkError::from_client)?;
    if args.json {
        print!("{}", render_json(&document)?);
    } else {
        print!("{}", render_recipes_human(&document));
    }
    Ok(())
}

fn dispatch_bench(host: &str, args: BenchArgs) -> Result<(), SparkError> {
    validate_selection_objective(&args.objective)?;
    let request = BenchRequest {
        model: args.model,
        recipe: args.recipe,
        objective: args.objective,
        dry_run: args.dry_run,
    };
    let client = load_client(host, args.config_dir)?;
    let key = idempotency_key(args.idempotency_key);
    if args.dry_run {
        return render_evaluation(
            &client
                .bench_plan(&key, &request)
                .map_err(SparkError::from_client)?,
            args.json,
        );
    }
    let operation = client
        .bench(&key, &request)
        .map_err(SparkError::from_client)?;
    let operation = client
        .follow_operation(&operation.id, 0)
        .map_err(SparkError::from_client)?;
    render_operation(&operation, args.json)
}

fn dispatch_tune(host: &str, args: TuneArgs) -> Result<(), SparkError> {
    validate_selection_objective(&args.objective)?;
    let request = TuneRequest {
        model: args.model,
        objective: args.objective,
        dry_run: args.dry_run,
    };
    let client = load_client(host, args.config_dir)?;
    let key = idempotency_key(args.idempotency_key);
    if args.dry_run {
        return render_evaluation(
            &client
                .tune_plan(&key, &request)
                .map_err(SparkError::from_client)?,
            args.json,
        );
    }
    let operation = client
        .tune(&key, &request)
        .map_err(SparkError::from_client)?;
    let operation = if args.detach {
        operation
    } else {
        client
            .follow_operation(&operation.id, 0)
            .map_err(SparkError::from_client)?
    };
    render_operation(&operation, args.json)
}

fn validate_selection_objective(objective: &str) -> Result<(), SparkError> {
    matches!(
        objective,
        "agent" | "interactive" | "long-context" | "retrieval"
    )
    .then_some(())
    .ok_or_else(|| {
        SparkError::usage("objective must be agent, interactive, long-context, or retrieval")
    })
}

fn render_evaluation(
    document: &CompatibilityEvaluationDocument,
    json: bool,
) -> Result<(), SparkError> {
    if json {
        print!("{}", render_json(document)?);
    } else {
        println!("functional compatibility {}", document.id);
        println!(
            "selected: {}",
            document.selected_recipe_id.as_deref().unwrap_or("none")
        );
        println!(
            "vLLM fallback: {}",
            document.fallback_recipe_id.as_deref().unwrap_or("none")
        );
        for candidate in &document.candidates {
            println!(
                "{}  {:?}  {}",
                candidate.engine_family, candidate.status, candidate.reason
            );
        }
    }
    Ok(())
}

fn dispatch_download(host: &str, args: DownloadArgs) -> Result<(), SparkError> {
    let request = DownloadRequest {
        repository: args.repository,
        revision: args.revision,
        alias: args.alias,
        update_alias: args.update_alias,
        dry_run: args.dry_run,
    };
    let client = load_client(host, args.config_dir)?;
    let key = idempotency_key(args.idempotency_key);
    if args.dry_run {
        let plan = client
            .download_plan(&key, &request)
            .map_err(SparkError::from_client)?;
        return render_or_print(args.json, &plan);
    }
    let operation = client
        .download(&key, &request)
        .map_err(SparkError::from_client)?;
    let operation = if args.detach {
        operation
    } else {
        client
            .follow_operation(&operation.id, 0)
            .map_err(SparkError::from_client)?
    };
    render_operation(&operation, args.json)
}

fn dispatch_serve(host: &str, args: ServeArgs) -> Result<(), SparkError> {
    validate_selection_objective(&args.objective)?;
    let client = load_client(host, args.config_dir)?;
    let key = idempotency_key(args.idempotency_key);
    if args.dry_run {
        let report = client
            .admission_plan(
                &key,
                &ServeAdmissionRequest {
                    model: args.model,
                    name: args.name,
                    recipe: args.recipe,
                    dry_run: true,
                },
            )
            .map_err(SparkError::from_client)?;
        if args.json {
            print!("{}", render_json(&report)?);
        } else {
            print!("{}", render_admission_human(&report));
        }
        return report.admitted.then_some(()).ok_or_else(|| SparkError {
            code: super::EXIT_REJECTED,
            msg: "Spark resource admission rejected the requested model".into(),
        });
    }
    let operation = client
        .serve(
            &key,
            &ServeRequest {
                model: args.model,
                name: args.name,
                recipe: args.recipe,
                objective: args.objective,
                allow_unverified: args.allow_unverified,
                dry_run: false,
            },
        )
        .map_err(SparkError::from_client)?;
    let operation = if args.detach {
        operation
    } else {
        client
            .follow_operation(&operation.id, 0)
            .map_err(SparkError::from_client)?
    };
    render_operation(&operation, args.json)
}

fn dispatch_instances(host: &str, args: ReadArgs) -> Result<(), SparkError> {
    let client = load_client(host, args.config_dir)?;
    let document = client.instances().map_err(SparkError::from_client)?;
    if args.json {
        print!("{}", render_json(&document)?);
    } else {
        for instance in &document.instances {
            println!(
                "{}  {}  desired={:?} observed={:?} healthy={}",
                instance.name,
                instance.model,
                instance.desired,
                instance.observed,
                instance.healthy
            );
        }
    }
    Ok(())
}

fn dispatch_stop(host: &str, args: StopArgs) -> Result<(), SparkError> {
    let client = load_client(host, args.config_dir)?;
    let key = idempotency_key(args.idempotency_key);
    let request = StopRequest {
        timeout_seconds: args.timeout_seconds,
        dry_run: args.dry_run,
    };
    if args.dry_run {
        let plan = client
            .stop_plan(&args.instance, &key, &request)
            .map_err(SparkError::from_client)?;
        return render_or_print(args.json, &plan);
    }
    let operation = client
        .stop_instance(&args.instance, &key, &request)
        .map_err(SparkError::from_client)?;
    let operation = client
        .follow_operation(&operation.id, 0)
        .map_err(SparkError::from_client)?;
    render_operation(&operation, args.json)
}

fn dispatch_logs(host: &str, args: LogsArgs) -> Result<(), SparkError> {
    let client = load_client(host, args.config_dir)?;
    let mut cursor = args.cursor;
    loop {
        let document = client
            .instance_logs(&args.instance, cursor, args.limit)
            .map_err(SparkError::from_client)?;
        if !args.follow || !document.lines.is_empty() {
            if args.json {
                print!("{}", render_json(&document)?);
            } else {
                for line in &document.lines {
                    println!("{line}");
                }
            }
        }
        if !args.follow {
            return Ok(());
        }
        cursor = document.next_cursor;
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn dispatch_models(host: &str, args: ReadArgs) -> Result<(), SparkError> {
    let client = load_client(host, args.config_dir)?;
    let models = client.list_models().map_err(SparkError::from_client)?;
    if args.json {
        print!("{}", render_json(&models)?);
    } else {
        render_model_list(&models);
    }
    Ok(())
}

fn dispatch_show(host: &str, args: ModelReadArgs) -> Result<(), SparkError> {
    let client = load_client(host, args.config_dir)?;
    let models = client.list_models().map_err(SparkError::from_client)?;
    let model = resolve_model(&models, &args.model)?;
    let model = client.model(&model.id).map_err(SparkError::from_client)?;
    if args.json {
        print!("{}", render_json(&model)?);
    } else {
        render_model(&model);
    }
    Ok(())
}

fn dispatch_remove(host: &str, args: RemoveArgs) -> Result<(), SparkError> {
    if !args.dry_run && !args.yes {
        return Err(SparkError::usage(
            "model removal requires --dry-run or --yes",
        ));
    }
    let client = load_client(host, args.config_dir)?;
    let models = client.list_models().map_err(SparkError::from_client)?;
    let model = resolve_model(&models, &args.model)?;
    let key = idempotency_key(args.idempotency_key);
    let request = RemoveModelRequest {
        dry_run: args.dry_run,
        confirmed: args.yes,
    };
    if args.dry_run {
        let plan = client
            .removal_plan(&model.id, &key, &request)
            .map_err(SparkError::from_client)?;
        return render_or_print(args.json, &plan);
    }
    let operation = client
        .remove_model(&model.id, &key, &request)
        .map_err(SparkError::from_client)?;
    render_operation(&operation, args.json)
}

fn resolve_model<'a>(
    document: &'a ModelListDocument,
    reference: &str,
) -> Result<&'a ModelDocument, SparkError> {
    let matches = document
        .models
        .iter()
        .filter(|model| {
            model.id == reference
                || model.canonical == reference
                || model.repository == reference
                || model.aliases.iter().any(|alias| alias == reference)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [model] => Ok(*model),
        [] => Err(SparkError::usage(format!(
            "model {reference:?} is not downloaded"
        ))),
        _ => Err(SparkError::usage(format!(
            "model {reference:?} is ambiguous; use an alias or canonical identity"
        ))),
    }
}

fn render_model_list(document: &ModelListDocument) {
    for model in &document.models {
        println!(
            "{}  {}  {}  {} bytes",
            model.id, model.repository, model.commit, model.logical_bytes
        );
    }
}

fn render_model(model: &ModelDocument) {
    println!("{}", model.canonical);
    println!("snapshot: {}", model.snapshot);
    println!("logical bytes: {}", model.logical_bytes);
    println!("unique bytes: {}", model.unique_bytes);
    println!("transport: {}", model.transport);
    if !model.aliases.is_empty() {
        println!("aliases: {}", model.aliases.join(","));
    }
    if !model.active_instances.is_empty() {
        println!("active: {}", model.active_instances.join(","));
    }
}

fn render_recipes_human(document: &RecipeCatalogDocument) -> String {
    let mut output = format!("Recipe catalog {}\n", document.catalog_sha256);
    if let Some(selection) = &document.selection {
        output.push_str(&format!(
            "selected {} ({:?})\n",
            selection.recipe_id, selection.reason
        ));
    } else if document.model_repository.is_some() {
        output.push_str("selected none\n");
    }
    for recipe in &document.recipes {
        output.push_str(&format!(
            "{}  {:?}  {}\n",
            recipe.id,
            recipe.status,
            if recipe.compatible {
                "compatible"
            } else {
                "unsupported"
            }
        ));
        for mismatch in &recipe.mismatches {
            output.push_str(&format!(
                "  mismatch {}: observed {}; requires {}\n",
                mismatch.field, mismatch.actual, mismatch.expected
            ));
        }
        for remediation in &recipe.remediation {
            output.push_str(&format!("  remediation: {remediation}\n"));
        }
    }
    output
}

fn render_admission_human(report: &super::resources::AdmissionReport) -> String {
    let mut output = format!(
        "Spark admission: {} (aggregate {} bytes; reserve {} bytes)\n",
        if report.admitted {
            "admitted"
        } else {
            "rejected"
        },
        report.aggregate_cold_start_bytes.unwrap_or(u64::MAX),
        report.policy.system_reserve_bytes
    );
    if let Some(selection) = &report.selection {
        output.push_str(&format!(
            "  selection: {}\n  recipe: {}\n  engine: {}\n  image: {}\n  fingerprint: {}\n",
            selection.selection_kind,
            selection.recipe_id,
            selection.engine,
            selection.image,
            selection.fingerprint
        ));
    }
    for code in &report.problem_codes {
        output.push_str(&format!("  refusal: {code}\n"));
    }
    output
}

fn dispatch_operations(host: &str, args: OperationsArgs) -> Result<(), SparkError> {
    if let Some(OperationsCommand::Cancel(cancel)) = args.command {
        if args.id.is_some() || args.follow {
            return Err(SparkError::usage(
                "operations cancel does not accept a second operation ID or --follow",
            ));
        }
        if cancel.dry_run {
            return render_or_print(
                cancel.json,
                &serde_json::json!({"schema":"sy.spark.dry-run/v1","action":"operation.cancel","operation_id":cancel.id}),
            );
        }
        let client = load_client(host, cancel.config_dir)?;
        let operation = client
            .cancel_operation(&cancel.id, &idempotency_key(cancel.idempotency_key))
            .map_err(SparkError::from_client)?;
        render_operation(&operation, cancel.json)
    } else {
        let client = load_client(host, args.config_dir)?;
        match args.id {
            Some(id) => {
                let operation = if args.follow {
                    client.follow_operation(&id, 0)
                } else {
                    client.operation(&id)
                }
                .map_err(SparkError::from_client)?;
                render_operation(&operation, args.json)
            }
            None if args.follow => Err(SparkError::usage(
                "operations --follow requires an operation ID",
            )),
            None => {
                let operations = client.list_operations().map_err(SparkError::from_client)?;
                if args.json {
                    print!("{}", render_json(&operations)?);
                } else {
                    render_operation_list(&operations);
                }
                Ok(())
            }
        }
    }
}

fn dispatch_tokens(host: &str, args: TokenArgs) -> Result<(), SparkError> {
    match args.command {
        TokenCommand::Create(args) => {
            let request = TokenCreateRequest {
                name: args.name,
                scopes: args.scopes,
                allowed_cidrs: args.allowed_cidrs,
                expires_at: args.expires_at,
                max_concurrent_inference: args.max_concurrent_inference,
            };
            if args.dry_run {
                return render_or_print(args.json, &request);
            }
            let client = load_client(host, args.config_dir)?;
            let created = client
                .create_token(&idempotency_key(args.idempotency_key), &request)
                .map_err(SparkError::from_client)?;
            if !args.detach && !created.operation.state.is_terminal() {
                client
                    .follow_operation(&created.operation.id, 0)
                    .map_err(SparkError::from_client)?;
            }
            render_created_token(&created, args.json)
        }
        TokenCommand::List(args) => {
            let client = load_client(host, args.config_dir)?;
            let tokens = client.list_tokens().map_err(SparkError::from_client)?;
            if args.json {
                print!("{}", render_json(&tokens)?);
            } else {
                render_token_list(&tokens);
            }
            Ok(())
        }
        TokenCommand::Revoke(args) => {
            if args.dry_run {
                return render_or_print(
                    args.json,
                    &serde_json::json!({"schema":"sy.spark.dry-run/v1","action":"token.revoke","token_id":args.id}),
                );
            }
            if !args.yes {
                return Err(SparkError::usage(
                    "token revoke requires --yes or --dry-run",
                ));
            }
            let client = load_client(host, args.config_dir)?;
            let operation = client
                .revoke_token(&args.id, &idempotency_key(args.idempotency_key))
                .map_err(SparkError::from_client)?;
            render_operation(&operation, args.json)
        }
    }
}

fn load_client(host: &str, config_dir: Option<PathBuf>) -> Result<SparkClient, SparkError> {
    SparkClient::load(&config_dir.unwrap_or_else(default_config_dir), host)
        .map_err(SparkError::from_client)
}

fn dispatch_client_config(host: &str, args: ClientConfigArgs) -> Result<(), SparkError> {
    let config_dir = args.config_dir.unwrap_or_else(default_config_dir);
    let spark = SparkClient::load(&config_dir, host).map_err(SparkError::from_client)?;
    let instances = spark.instances().map_err(SparkError::from_client)?;
    let instance = instances
        .instances
        .iter()
        .find(|value| value.id == args.instance || value.name == args.instance)
        .ok_or_else(|| SparkError::usage("Spark instance was not found"))?;
    if !instance.healthy || instance.endpoint.is_none() {
        return Err(SparkError::usage(
            "Spark instance is not healthy and published",
        ));
    }
    match args.client {
        ClientKind::Codex => {
            let config =
                client::codex_client_config(&config_dir, host, &instance.name, &instance.model)
                    .map_err(SparkError::from_client)?;
            if args.json {
                print!("{}", render_json(&config)?);
            } else {
                print!(
                    "{}\n# export {} from the protected inference-token source\n# export {}={}\n",
                    config.toml,
                    config.env_key,
                    config.ca_env_key,
                    config.ca_path.display()
                );
            }
        }
        ClientKind::ClaudeCode => {
            let config = client::claude_code_client_config(
                &config_dir,
                host,
                &instance.name,
                &instance.model,
            )
            .map_err(SparkError::from_client)?;
            if args.json {
                print!("{}", render_json(&config)?);
            } else {
                println!(
                    "{}# export {} from the protected inference-token source",
                    config.shell, config.secret_env_key
                );
            }
        }
    }
    Ok(())
}

fn idempotency_key(value: Option<String>) -> String {
    value.unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn render_or_print<T: Serialize>(json: bool, value: &T) -> Result<(), SparkError> {
    if json {
        print!("{}", render_json(value)?);
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(value)
                .map_err(|error| SparkError::usage(format!("encode Spark output: {error}")))?
        );
    }
    Ok(())
}

fn render_operation(operation: &OperationDocument, json: bool) -> Result<(), SparkError> {
    if json {
        print!("{}", render_json(operation)?);
    } else {
        println!(
            "{}  {:?}  {}",
            operation.id, operation.state, operation.progress.message
        );
    }
    Ok(())
}

fn render_operation_list(document: &OperationListDocument) {
    for operation in &document.operations {
        println!(
            "{}  {:?}  {}",
            operation.id, operation.state, operation.kind
        );
    }
}

fn render_created_token(document: &TokenCreatedDocument, json: bool) -> Result<(), SparkError> {
    if json {
        print!("{}", render_json(document)?);
    } else {
        println!("token {} ({})", document.token.id, document.token.name);
        if let Some(secret) = &document.bearer_token {
            println!("{secret}");
            eprintln!("This bearer token is shown once; store it in a mode-0600 credential file.");
        }
    }
    Ok(())
}

fn render_token_list(document: &TokenListDocument) {
    for token in &document.tokens {
        let scopes = token
            .scopes
            .iter()
            .map(super::wire::TokenScope::as_str)
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{}  {}  {}{}",
            token.id,
            token.name,
            scopes,
            if token.revoked_at.is_some() {
                "  revoked"
            } else {
                ""
            }
        );
    }
}

fn dispatch_read<T: serde::de::DeserializeOwned + Serialize>(
    host: &str,
    args: ReadArgs,
    route: &str,
    human: fn(&T),
) -> Result<(), SparkError> {
    let config_dir = args.config_dir.unwrap_or_else(default_config_dir);
    let client = SparkClient::load(&config_dir, host).map_err(SparkError::from_client)?;
    let document = client
        .get_json::<T>(route)
        .map_err(SparkError::from_client)?;
    if args.json {
        print!("{}", render_json(&document)?);
    } else {
        human(&document);
    }
    Ok(())
}

fn default_config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("sy")
}

fn render_status(value: &StatusDocument) {
    println!("Spark agent {} — executor {}", value.agent, value.executor);
    for reason in &value.degraded_reasons {
        println!("  degraded: {}", reason.detail);
    }
}

fn render_doctor(value: &DoctorDocument) {
    for check in &value.checks {
        println!("{}: {} — {}", check.status, check.code, check.detail);
    }
}

fn render_certificate(value: &CertificateStatusDocument) {
    println!(
        "Spark certificate: {}",
        if value.valid { "valid" } else { "invalid" }
    );
}

#[cfg(feature = "spark-agent")]
fn dispatch_agent(host: String, args: AgentArgs) -> Result<(), SparkError> {
    if host != "agent" {
        return Err(SparkError::usage(
            "the agent entrypoint is fixed to `spark agent run-agent`",
        ));
    }
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| SparkError::usage(format!("start Spark agent runtime: {error}")))?;
    runtime
        .block_on(super::agent::serve(
            &args.config,
            &args.certificate,
            &args.key,
            &args.token,
            &args.hf_token,
        ))
        .map_err(|error| SparkError {
            code: super::EXIT_INTERNAL,
            msg: format!("Spark agent failed: {error}"),
        })
}

#[cfg(feature = "spark-agent")]
fn dispatch_executor(host: String, args: ExecutorArgs) -> Result<(), SparkError> {
    if host != "executor" {
        return Err(SparkError::usage(
            "the executor entrypoint is fixed to `spark executor run-executor`",
        ));
    }
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| SparkError::usage(format!("start Spark executor runtime: {error}")))?;
    runtime
        .block_on(super::executor::serve(&args.config))
        .map_err(|error| SparkError {
            code: super::EXIT_INTERNAL,
            msg: format!("Spark executor failed: {error}"),
        })
}

#[cfg(feature = "spark-agent")]
fn dispatch_activate(host: String, args: ActivateArgs) -> Result<(), SparkError> {
    use std::io::Write;
    if host != "bootstrap" {
        return Err(SparkError::usage(
            "the activation entrypoint is fixed to `spark bootstrap activate`",
        ));
    }
    let (report, material) = install::activate_from_files(&install::ActivateRequest {
        root: PathBuf::from("/"),
        executable: args.executable,
        signature: args.signature,
        public_key: args.public_key,
        manifest: args.manifest,
        manifest_sha256: args.manifest_sha256,
        version: args.version,
        listen_address: args.listen_address,
        hostname: args.hostname,
        active_lsm: args.active_lsm,
    })
    .map_err(SparkError::from_install)?;
    let mut stdout = std::io::stdout().lock();
    install::write_bootstrap_channel(&report, &material, &mut stdout)
        .map_err(SparkError::from_install)?;
    stdout.flush().map_err(|error| {
        SparkError::usage(format!("flush protected SSH bootstrap channel: {error}"))
    })
}

fn dispatch_install(host: String, args: InstallArgs, operation: &str) -> Result<(), SparkError> {
    if host == "bootstrap" {
        return Err(SparkError::usage(
            "bootstrap is reserved for the fixed internal inspector",
        ));
    }
    let resolved = resolve_install(&args, &ProcessEnv)?;
    let json = resolved.json;
    let probe_path = resolved.probe.clone();
    let mut manifest = install::inspect_and_plan(
        &OpenSshRunner,
        InstallRequest {
            host_alias: host,
            probe_path: resolved.probe,
            listen_address: resolved.listen_address,
            listen_port: resolved.listen_port,
        },
    )
    .map_err(SparkError::from_install)?;
    if operation == "upgrade" && !manifest.inventory.existing_installation.present {
        return Err(SparkError {
            code: super::EXIT_REJECTED,
            msg: "upgrade requires an existing Spark installation; use install first".into(),
        });
    }
    manifest.operation = operation.into();
    manifest.approval_sha256 =
        install::manifest_approval_sha256(&manifest).map_err(SparkError::usage)?;
    if resolved.yes {
        let signature = resolved
            .release_signature
            .as_deref()
            .ok_or_else(|| SparkError::usage("--release-signature is required with --yes"))?;
        let public_key = resolved
            .release_public_key
            .as_deref()
            .ok_or_else(|| SparkError::usage("--release-public-key is required with --yes"))?;
        let outcome = install::activate_over_ssh(&install::RemoteActivation {
            host_alias: &manifest.host_alias,
            executable: &probe_path,
            signature,
            public_key,
            version: env!("CARGO_PKG_VERSION"),
            executable_sha256: &manifest.probe.local_sha256,
            listen_address: &manifest.listen_address,
            hostname: &manifest.inventory.hostname,
            active_lsm: &format!(
                "{}:{}",
                manifest.inventory.lsm.kind, manifest.inventory.lsm.mode
            ),
            manifest: &manifest,
        })
        .map_err(SparkError::from_install)?;
        let address: std::net::IpAddr = manifest
            .listen_address
            .parse()
            .map_err(|_| SparkError::usage("inspected Spark listen address is invalid"))?;
        super::client::store_bootstrap(
            &resolved.config_dir,
            &manifest.host_alias,
            &format!(
                "https://{}",
                std::net::SocketAddr::new(address, manifest.listen_port)
            ),
            &outcome.material,
        )
        .map_err(SparkError::from_client)?;
        if operation == "upgrade" {
            let healthy = SparkClient::load(&resolved.config_dir, &manifest.host_alias)
                .and_then(|client| client.get_json::<StatusDocument>("api/sy.spark/v1/status"))
                .is_ok_and(|status| !status.read_only && status.degraded_reasons.is_empty());
            if !healthy {
                let _ = install::maintenance_over_ssh(
                    &manifest.host_alias,
                    install::MaintenanceCommand::Rollback,
                    false,
                );
                return Err(SparkError {
                    code: super::EXIT_REJECTED,
                    msg: "upgraded control plane failed semantic health; automatic rollback requested"
                        .into(),
                });
            }
        }
        record_install_execution(
            &mut manifest,
            outcome.changed,
            outcome.active_release.to_string_lossy().into_owned(),
            outcome
                .preceding_release
                .map(|path| path.to_string_lossy().into_owned()),
        );
    }
    if json {
        print!("{}", render_json(&manifest)?);
    } else {
        println!(
            "Spark {operation} {} for {}",
            if resolved.yes { "complete" } else { "dry-run" },
            manifest.host_alias
        );
        println!(
            "  protected fingerprint: {}",
            manifest.protected_before.sha256
        );
        println!("  planned assets: {}", manifest.assets.len());
        println!(
            "  installation performed: {}",
            match manifest.execution {
                super::wire::InstallExecution::Planned => "no",
                super::wire::InstallExecution::Applied { changed: true, .. } => "yes (changed)",
                super::wire::InstallExecution::Applied { changed: false, .. } => "yes (no-change)",
            }
        );
        println!("  protected DGX updates: rejected");
    }
    Ok(())
}

fn maintenance_mode(dry_run: bool, yes: bool, action: &str) -> Result<bool, SparkError> {
    if dry_run == yes {
        return Err(SparkError::usage(format!(
            "{action} requires exactly one of --dry-run or --yes"
        )));
    }
    Ok(dry_run)
}

fn dispatch_rollback(host: String, args: MaintenanceArgs) -> Result<(), SparkError> {
    let dry_run = maintenance_mode(args.dry_run, args.yes, "rollback")?;
    if host == "bootstrap" {
        #[cfg(feature = "spark-agent")]
        {
            let report = install::rollback_installed(dry_run).map_err(SparkError::from_install)?;
            return render_or_print(args.json, &report);
        }
        #[cfg(not(feature = "spark-agent"))]
        return Err(SparkError::usage(
            "bootstrap rollback requires the ARM64 spark-agent artifact",
        ));
    }
    let response =
        install::maintenance_over_ssh(&host, install::MaintenanceCommand::Rollback, dry_run)
            .map_err(SparkError::from_install)?;
    let report: install::MaintenanceReport = serde_json::from_str(&response)
        .map_err(|_| SparkError::usage("Spark returned an incompatible rollback report"))?;
    render_or_print(args.json, &report)
}

fn dispatch_certificate_rotate(
    host: String,
    args: CertificateRotateArgs,
) -> Result<(), SparkError> {
    let dry_run = maintenance_mode(args.dry_run, args.yes, "certificate rotation")?;
    if host == "bootstrap" {
        #[cfg(feature = "spark-agent")]
        {
            let report = install::rotate_installed_certificate(dry_run, args.ca)
                .map_err(SparkError::from_install)?;
            return render_or_print(args.json, &report);
        }
        #[cfg(not(feature = "spark-agent"))]
        return Err(SparkError::usage(
            "certificate rotation requires the ARM64 spark-agent artifact",
        ));
    }
    let command = if args.ca {
        install::MaintenanceCommand::RotateCa
    } else {
        install::MaintenanceCommand::RotateLeaf
    };
    let response =
        install::maintenance_over_ssh(&host, command, dry_run).map_err(SparkError::from_install)?;
    let report: install::CertificateRotationReport = serde_json::from_str(&response)
        .map_err(|_| SparkError::usage("Spark returned an incompatible certificate report"))?;
    if report.applied && report.client_repin_required {
        let ca = report.ca_certificate_pem.as_deref().ok_or_else(|| {
            SparkError::usage("CA rotation did not return its public certificate over SSH")
        })?;
        client::replace_ca_pin(
            &args.config_dir.unwrap_or_else(default_config_dir),
            &host,
            ca,
            &report.ca_certificate_sha256,
        )
        .map_err(SparkError::from_client)?;
    }
    render_or_print(args.json, &report)
}

fn record_install_execution(
    manifest: &mut super::wire::InstallManifest,
    changed: bool,
    active_release: String,
    preceding_release: Option<String>,
) {
    manifest.dry_run = false;
    manifest.installation_performed = changed;
    manifest.execution = super::wire::InstallExecution::Applied {
        changed,
        active_release,
        preceding_release,
    };
}

fn dispatch_bootstrap(host: String) -> Result<(), SparkError> {
    if host != "bootstrap" {
        return Err(SparkError::usage(
            "the inspect entrypoint is fixed to `spark bootstrap inspect`",
        ));
    }
    #[cfg(feature = "spark-agent")]
    {
        let inventory = install::bootstrap_inventory().map_err(SparkError::from_install)?;
        print!("{}", render_json(&inventory)?);
        Ok(())
    }
    #[cfg(not(feature = "spark-agent"))]
    {
        Err(SparkError::usage(
            "bootstrap inspect requires the ARM64 spark-agent artifact",
        ))
    }
}

#[derive(Debug)]
pub struct SparkError {
    pub code: i32,
    pub msg: String,
}

impl SparkError {
    fn usage(msg: impl Into<String>) -> Self {
        Self {
            code: EXIT_USAGE,
            msg: msg.into(),
        }
    }

    fn from_install(error: install::InstallError) -> Self {
        let code = match error.kind {
            InstallErrorKind::Configuration => EXIT_USAGE,
            InstallErrorKind::Unreachable => EXIT_UNREACHABLE,
        };
        Self {
            code,
            msg: error.message,
        }
    }

    fn from_client(error: super::client::ClientError) -> Self {
        Self {
            code: error.code,
            msg: error.message,
        }
    }

    pub fn exit(&self) -> ! {
        eprintln!("error: {}", self.msg);
        std::process::exit(self.code);
    }
}

impl fmt::Display for SparkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.msg)
    }
}

impl std::error::Error for SparkError {}

#[cfg(test)]
mod tests {
    use super::{render_json, resolve_install, InstallArgs, MapEnv};
    use clap::Parser;
    use std::{collections::BTreeMap, path::PathBuf};

    #[test]
    fn bootstrap_inspector_has_one_fixed_entrypoint() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let parsed = TestCli::try_parse_from(["sy", "bootstrap", "inspect"]).unwrap();
        assert!(matches!(parsed.spark.command, super::SparkCommand::Inspect));
        assert!(TestCli::try_parse_from(["sy", "bootstrap", "bootstrap", "inspect"]).is_err());
    }

    #[test]
    fn bench_and_tune_expose_bounded_functional_options() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let bench = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "bench",
            "ornith-1.5:9b",
            "--recipe",
            "ornith-vllm",
            "--dry-run",
            "--json",
        ])
        .unwrap();
        assert!(matches!(bench.spark.command, super::SparkCommand::Bench(_)));
        let tune = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "tune",
            "ornith-1.5:9b",
            "--objective",
            "agent",
            "--detach",
            "--json",
        ])
        .unwrap();
        assert!(matches!(tune.spark.command, super::SparkCommand::Tune(_)));
    }

    #[test]
    fn maintenance_commands_require_explicit_modes() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }

        let upgrade =
            TestCli::try_parse_from(["sy", "dgx-spark", "upgrade", "--dry-run", "--json"]).unwrap();
        assert!(matches!(
            upgrade.spark.command,
            super::SparkCommand::Upgrade(_)
        ));

        let rollback =
            TestCli::try_parse_from(["sy", "dgx-spark", "rollback", "--yes", "--json"]).unwrap();
        assert!(matches!(
            rollback.spark.command,
            super::SparkCommand::Rollback(_)
        ));

        let rotate =
            TestCli::try_parse_from(["sy", "dgx-spark", "cert", "rotate", "--dry-run", "--json"])
                .unwrap();
        assert!(matches!(
            rotate.spark.command,
            super::SparkCommand::Cert(super::CertArgs {
                command: super::CertCommand::Rotate(_)
            })
        ));
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn executor_has_one_fixed_unix_service_entrypoint() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let parsed = TestCli::try_parse_from([
            "sy",
            "executor",
            "run-executor",
            "--config",
            "/etc/sy/spark-executor.toml",
        ])
        .unwrap();
        assert!(matches!(
            parsed.spark.command,
            super::SparkCommand::RunExecutor(_)
        ));
        assert!(TestCli::try_parse_from(["sy", "executor", "run-executor", "extra"]).is_err());
    }

    #[test]
    fn install_dry_run_obeys_flag_env_default_precedence() {
        let env = MapEnv(BTreeMap::from([
            ("SY_SPARK_PROBE".into(), "/env/probe".into()),
            ("SY_SPARK_LISTEN_ADDRESS".into(), "10.1.30.9".into()),
            ("SY_SPARK_LISTEN_PORT".into(), "9443".into()),
        ]));
        let flags = InstallArgs {
            dry_run: true,
            yes: false,
            json: true,
            probe: Some(PathBuf::from("/flag/probe")),
            listen_address: Some("10.1.30.143".into()),
            listen_port: None,
            release_signature: None,
            release_public_key: None,
            config_dir: Some(PathBuf::from("/flag/config")),
        };
        let resolved = resolve_install(&flags, &env).unwrap();
        assert_eq!(resolved.probe, PathBuf::from("/flag/probe"));
        assert_eq!(resolved.listen_address.as_deref(), Some("10.1.30.143"));
        assert_eq!(resolved.listen_port, 9443);
        assert_eq!(
            render_json(&resolved).unwrap(),
            concat!(
                "{\n  \"dry_run\": true,\n  \"yes\": false,\n  \"json\": true,\n  \"probe\": \"/flag/probe\",\n  ",
                "\"listen_address\": \"10.1.30.143\",\n  \"listen_port\": 9443,\n  \"release_signature\": null,\n  \"release_public_key\": null,\n  \"config_dir\": ",
                "\"/flag/config\"\n}\n"
            )
        );

        let defaults =
            resolve_install(&InstallArgs::dry_run_for_test(), &MapEnv::default()).unwrap();
        assert_eq!(
            defaults.probe,
            PathBuf::from("/usr/libexec/sy/spark-bootstrap-aarch64")
        );
        assert_eq!(defaults.listen_port, 9843);
    }

    #[test]
    fn install_output_keeps_approval_hash_and_reports_only_remote_changes() {
        use crate::spark::{
            install::{build_manifest, manifest_approval_sha256, PlanOptions},
            wire::{decode_inventory, InstallExecution},
        };
        let inventory =
            decode_inventory(crate::spark::install::tests_fixture::INVENTORY.as_bytes()).unwrap();
        let mut manifest = build_manifest(
            inventory,
            PlanOptions {
                host_alias: "dgx-spark".into(),
                listen_address: "10.1.30.143".into(),
                listen_port: 9843,
                probe_remote_path: format!("/tmp/sy-spark-bootstrap-{}", "a".repeat(64)),
                probe_sha256: "a".repeat(64),
                probe_removed: true,
            },
        )
        .unwrap();
        let approval = manifest.approval_sha256.clone();
        assert!(!manifest.installation_performed);
        super::record_install_execution(
            &mut manifest,
            false,
            "opt/sy-spark/releases/0.1.0".into(),
            Some("releases/0.1.0".into()),
        );
        assert!(!manifest.installation_performed);
        assert!(!manifest.dry_run);
        assert_eq!(manifest_approval_sha256(&manifest).unwrap(), approval);
        let rendered = render_json(&manifest).unwrap();
        assert!(rendered.contains("\"changed\": false"));
        assert!(rendered.contains(&approval));
        assert!(!rendered.contains("bootstrap-token"));
        super::record_install_execution(
            &mut manifest,
            true,
            "opt/sy-spark/releases/0.1.0".into(),
            None,
        );
        assert!(manifest.installation_performed);
        assert!(!manifest.dry_run);
        assert!(matches!(
            manifest.execution,
            InstallExecution::Applied { changed: true, .. }
        ));
        assert_eq!(manifest_approval_sha256(&manifest).unwrap(), approval);
    }

    #[test]
    fn operation_and_token_commands_parse_the_documented_journey() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let cancel = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "operations",
            "cancel",
            "01K00000000000000000000000",
            "--dry-run",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cancel.spark.command,
            super::SparkCommand::Operations(super::OperationsArgs {
                command: Some(super::OperationsCommand::Cancel(_)),
                ..
            })
        ));
        let create = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "token",
            "create",
            "--name",
            "reader",
            "--scope",
            "models:read",
            "--scope",
            "operations:read",
            "--detach",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            create.spark.command,
            super::SparkCommand::Token(super::TokenArgs {
                command: super::TokenCommand::Create(_),
                ..
            })
        ));
    }

    #[test]
    fn recipes_explains_all_mismatches_without_mutation() {
        use crate::spark::wire::{
            RecipeCatalogDocument, RecipeCompatibilityDocument, RecipeEvidenceDocument,
            RecipeMismatchDocument, RecipeStatus,
        };
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let parsed = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "recipes",
            "ornith-ai/Ornith-1.5-9B",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            parsed.spark.command,
            super::SparkCommand::Recipes(super::RecipesArgs {
                model: Some(_),
                json: true,
                ..
            })
        ));
        let document = RecipeCatalogDocument {
            schema: "sy.spark.recipe-catalog/v1".into(),
            catalog_sha256: format!("sha256:{}", "a".repeat(64)),
            model_repository: Some("ornith-ai/Ornith-1.5-9B".into()),
            model_commit: Some("4".repeat(40)),
            objective: "agent".into(),
            selection: None,
            recipes: vec![RecipeCompatibilityDocument {
                id: "ornith-vllm".into(),
                version: 1,
                status: RecipeStatus::UpstreamVerified,
                model_repository: "ornith-ai/Ornith-1.5-9B".into(),
                model_commits: vec!["4".repeat(40)],
                engine: "vllm".into(),
                engine_version: "0.19.1".into(),
                image: format!("vllm/vllm-openai@sha256:{}", "f".repeat(64)),
                compatible: false,
                mismatches: vec![RecipeMismatchDocument {
                    field: "host.driver".into(),
                    actual: "579".into(),
                    expected: "580".into(),
                }],
                capabilities: vec!["text_generation".into()],
                resources: crate::spark::wire::RecipeResourceEnvelopeDocument {
                    image_bytes: 1,
                    startup_peak_bytes: 2,
                    steady_peak_bytes: 1,
                    compile_cache_bytes: 1,
                },
                evidence: RecipeEvidenceDocument {
                    source_url: "https://github.com/vllm-project/vllm".into(),
                    source_commit: "b".repeat(40),
                    upstream_recipe_commit: "c".repeat(40),
                    host_fingerprint: format!("sha256:{}", "d".repeat(64)),
                    quality: "upstream".into(),
                    stability_seconds: 0,
                    verified_at: "2026-08-24T00:00:00Z".into(),
                    expires_at: None,
                },
                remediation: vec!["install a compatible signed recipe".into()],
                fingerprint: format!("sha256:{}", "e".repeat(64)),
                specialized_toggles: 0,
            }],
        };
        let json = super::render_json(&document).unwrap();
        let human = super::render_recipes_human(&document);
        for evidence in ["ornith-vllm", "host.driver", "579", "580"] {
            assert!(json.contains(evidence) && human.contains(evidence));
        }
        assert!(!human.contains("download") && !human.contains("start engine"));
    }

    #[test]
    fn admission_human_output_exposes_the_complete_selected_fallback() {
        use crate::spark::resources::{
            AdmissionReport, AdmissionSelection, HostResourceSnapshot, ResourcePolicy,
        };
        const IMAGE: &str = "vllm/vllm-openai@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        const FINGERPRINT: &str =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let report = AdmissionReport {
            schema: "sy.spark.admission-report/v1".into(),
            admitted: true,
            problem_codes: Vec::new(),
            aggregate_cold_start_bytes: Some(42),
            reboot_capacity_bytes: Some(43),
            live_available_after_start_bytes: Some(44),
            disk_available_after_start_bytes: Some(45),
            policy: ResourcePolicy {
                system_reserve_bytes: 8,
                emergency_available_floor_bytes: 8,
                disk_reserve_bytes: 100,
                startup_guard_interval_ms: 500,
                steady_guard_interval_ms: 2_000,
                emergency_consecutive_samples: 3,
                memory_full_psi_avg10_percent: 2.0,
            },
            snapshot: HostResourceSnapshot {
                schema: "sy.spark.resources.snapshot/v1".into(),
                observed_at_unix_ms: 1,
                mem_total_bytes: Some(2),
                mem_available_bytes: Some(3),
                memory_full_psi_avg10_percent: Some(0.0),
                swap_in_pages_delta: Some(0),
                disk_available_bytes: Some(4),
            },
            selection: Some(AdmissionSelection {
                recipe_id: "ornith-1.5-9b-vllm-0.19.1".into(),
                selection_kind: "verified_vllm_fallback".into(),
                engine: "vllm".into(),
                image: IMAGE.into(),
                fingerprint: FINGERPRINT.into(),
            }),
        };

        assert_eq!(
            super::render_admission_human(&report),
            format!(
                "Spark admission: admitted (aggregate 42 bytes; reserve 8 bytes)\n  selection: verified_vllm_fallback\n  recipe: ornith-1.5-9b-vllm-0.19.1\n  engine: vllm\n  image: {IMAGE}\n  fingerprint: {FINGERPRINT}\n"
            )
        );
    }

    #[test]
    fn model_commands_parse_the_complete_acquire_inventory_and_removal_journey() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let download = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "download",
            "ornith-ai/Ornith-1.5-9B",
            "--revision",
            "489cb97981b8654bcfcf30ce1f94ed1b62e07b53",
            "--alias",
            "ornith-1.5:9b",
            "--detach",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            download.spark.command,
            super::SparkCommand::Download(super::DownloadArgs {
                detach: true,
                json: true,
                ..
            })
        ));
        let remove =
            TestCli::try_parse_from(["sy", "dgx-spark", "rm", "tiny:test", "--dry-run", "--json"])
                .unwrap();
        assert!(matches!(
            remove.spark.command,
            super::SparkCommand::Rm(super::RemoveArgs {
                dry_run: true,
                json: true,
                ..
            })
        ));
        let serve = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "serve",
            "ornith-1.5:9b",
            "--dry-run",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            serve.spark.command,
            super::SparkCommand::Serve(super::ServeArgs {
                dry_run: true,
                json: true,
                ..
            })
        ));
    }

    #[test]
    fn codex_client_config_command_is_explicit_and_read_only() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let parsed = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "client-config",
            "ornith",
            "--client",
            "codex",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            parsed.spark.command,
            super::SparkCommand::ClientConfig(super::ClientConfigArgs {
                client: super::ClientKind::Codex,
                json: true,
                ..
            })
        ));
        let claude = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "client-config",
            "ornith",
            "--client",
            "claude-code",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            claude.spark.command,
            super::SparkCommand::ClientConfig(super::ClientConfigArgs {
                client: super::ClientKind::ClaudeCode,
                json: true,
                ..
            })
        ));
    }
}
