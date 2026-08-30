//! CLIG surface for workstation bootstrap planning.

use std::{env, fmt, path::PathBuf, time::Duration};

use clap::{Args, Subcommand};
use serde::Serialize;

use super::{
    client::{self, SparkClient},
    wire::{
        CertificateStatusDocument, DoctorDocument, DownloadRequest, ModelArtifactSelectorDocument,
        ModelDocument, ModelListDocument, OperationDocument, OperationListDocument,
        RemoveModelRequest, ServeAdmissionRequest, ServeRequest, StatusDocument, StopRequest,
        TokenCreateRequest, TokenCreatedDocument, TokenListDocument,
    },
    EXIT_USAGE,
};
use super::{
    install::{self, InstallErrorKind, InstallRequest, OpenSshRunner},
    EXIT_UNREACHABLE,
};

const DEFAULT_LISTEN_PORT: u16 = 9843;

fn default_probe(env: &dyn EnvSource) -> PathBuf {
    let data = env
        .get("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|| PathBuf::from("/usr/local/share"));
    data.join("sy/spark-release/sy-aarch64")
}

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
        after_help = "Examples:\n  sy spark dgx-spark install --dry-run --json\n  sy spark dgx-spark install --yes --release-manifest SHA256SUMS --release-signature SHA256SUMS.minisig --release-public-key sy-release.pub\n\nEnvironment:\n  SY_SPARK_DRY_RUN, SY_SPARK_YES, SY_SPARK_JSON, SY_SPARK_PROBE, SY_SPARK_RELEASE_MANIFEST, SY_SPARK_LISTEN_ADDRESS, SY_SPARK_LISTEN_PORT, SY_SPARK_RELEASE_SIGNATURE, SY_SPARK_RELEASE_PUBLIC_KEY, SY_SPARK_CONFIG_DIR\n  Flags override environment values, which override declarative defaults.\n\nAuthentication:\n  OpenSSH owns known_hosts, agents, hardware tokens, keyboard-interactive and interactive password prompts. Credentials are never accepted as sy arguments or stored by sy.\n\nExit codes:\n  0 success; 2 usage/local configuration; 4 OpenSSH/SFTP/agent unreachable, TLS identity mismatch, or authentication failure.\n\nSecurity:\n  Dry-run uploads one content-addressed probe, invokes only `spark bootstrap inspect`, verifies its hash, and removes that exact path. Approved install uploads only signed content-addressed release inputs and invokes the fixed bootstrap activation entrypoint. No arbitrary remote command is accepted."
    )]
    Install(InstallArgs),
    /// Upgrade the signed control plane side by side without stopping engines.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark upgrade --dry-run --json\n  sy spark dgx-spark upgrade --yes --release-manifest SHA256SUMS --release-signature SHA256SUMS.minisig --release-public-key sy-release.pub --json\n\nEnvironment:\n  SY_SPARK_DRY_RUN, SY_SPARK_YES, SY_SPARK_JSON, SY_SPARK_PROBE, SY_SPARK_RELEASE_MANIFEST, SY_SPARK_RELEASE_SIGNATURE, SY_SPARK_RELEASE_PUBLIC_KEY, SY_SPARK_CONFIG_DIR\n\nExit codes:\n  0 success; 2 usage/local configuration; 3 compatibility or safety rejection; 4 SSH, TLS, or agent unreachable."
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
    /// Acquire and verify one immutable Hugging Face model snapshot.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark download ornith-1.5:35b --dry-run --json\n  sy spark dgx-spark download ornith-1.5:35b\n  sy spark dgx-spark download owner/model --revision <commit> --artifact model.gguf --auxiliary projector=mmproj.gguf --alias model:q4\n\nAuxiliaries require ROLE=PATH. ROLE is a lowercase identifier that the selected engine configuration must bind or explicitly ignore.\n\nEnvironment:\n  SY_SPARK_REVISION, SY_SPARK_ALIAS, SY_SPARK_ARTIFACT, SY_SPARK_AUXILIARY, SY_SPARK_UPDATE_ALIAS, SY_SPARK_DETACH, SY_SPARK_DRY_RUN, SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    Download(DownloadArgs),
    /// Start one managed model with the configured inference engine.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark serve ornith-1.5:35b\n  sy spark dgx-spark serve ornith-1.5:35b --dry-run --json\n\nServe starts the configured digest-pinned engine for any verified compatible model. The dry-run performs admission without Docker or GPU side effects.\n\nEnvironment:\n  SY_SPARK_INSTANCE_NAME, SY_SPARK_DETACH, SY_SPARK_DRY_RUN, SY_SPARK_JSON, SY_SPARK_IDEMPOTENCY_KEY, SY_SPARK_CONFIG_DIR\n\nExit codes:\n  0 success; 1 unexpected failure; 2 usage/local configuration; 3 remote policy/state rejection; 4 unreachable, TLS pin mismatch, or authentication failure."
    )]
    Serve(ServeArgs),
    /// Configure and launch a local coding agent against a managed Spark model.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark launch codex --model ornith-1.5:35b\n  sy spark dgx-spark launch claude --model ornith-1.5:35b -- --permission-mode plan\n  sy spark dgx-spark launch opencode --config --model ornith-1.5:35b\n  sy spark dgx-spark launch codex --model ornith-1.5:35b --dry-run --json\n\nArguments after `--` are passed directly to the selected local agent without a shell. The Spark administrator credential is never given to the child process.\n\nEnvironment:\n  SY_SPARK_LAUNCH_MODEL, SY_SPARK_LAUNCH_CONFIG, SY_SPARK_LAUNCH_RESTORE, SY_SPARK_YES, SY_SPARK_DRY_RUN, SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    Launch(LaunchArgs),
    /// List currently active managed model processes.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark ps\n  sy spark dgx-spark ps --json\n\nThe default output is a compact table. Absent and failed historical instances are omitted.\n\nEnvironment:\n  SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    Ps(ReadArgs),
    /// Read bounded redacted logs for one managed instance.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark logs ornith --limit 100\n  sy spark dgx-spark logs ornith --json\n\nEnvironment:\n  SY_SPARK_LOG_CURSOR, SY_SPARK_LOG_LIMIT, SY_SPARK_FOLLOW, SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    Logs(LogsArgs),
    /// Persist stopped intent, drain, and remove one managed instance.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark stop ornith --dry-run --json\n  sy spark dgx-spark stop ornith\n\nEnvironment:\n  SY_SPARK_STOP_TIMEOUT_SECONDS, SY_SPARK_DRY_RUN, SY_SPARK_JSON, SY_SPARK_IDEMPOTENCY_KEY, SY_SPARK_CONFIG_DIR"
    )]
    Stop(StopArgs),
    /// List verified local models available to run.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark ls\n  sy spark dgx-spark ls --json\n\nThe default output is a compact table. Use `show` or `--json` for complete identity and provenance.\n\nEnvironment:\n  SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    Ls(ReadArgs),
    /// Show immutable identity, provenance, aliases, and references for one model.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark show ornith-1.5:35b\n  sy spark dgx-spark show ornith-1.5:35b --json\n\nEnvironment:\n  SY_SPARK_JSON, SY_SPARK_CONFIG_DIR"
    )]
    Show(ModelReadArgs),
    /// Preview or remove only unreferenced native-cache model data.
    #[command(
        after_help = "Examples:\n  sy spark dgx-spark rm ornith-1.5:35b --dry-run --json\n  sy spark dgx-spark rm ornith-1.5:35b --yes\n\nEnvironment:\n  SY_SPARK_YES, SY_SPARK_DRY_RUN, SY_SPARK_JSON, SY_SPARK_IDEMPOTENCY_KEY, SY_SPARK_CONFIG_DIR"
    )]
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
pub struct DownloadArgs {
    pub repository: String,
    #[arg(long, default_value = "main", env = "SY_SPARK_REVISION")]
    pub revision: String,
    #[arg(long, env = "SY_SPARK_ALIAS")]
    pub alias: Option<String>,
    /// Override automatic artifact selection with an exact primary path.
    #[arg(long, env = "SY_SPARK_ARTIFACT")]
    pub artifact: Option<String>,
    /// Exact ROLE=PATH auxiliary; repeat for every engine-bound artifact.
    #[arg(long, env = "SY_SPARK_AUXILIARY", value_delimiter = ',')]
    pub auxiliary: Vec<ModelArtifactSelectorDocument>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LaunchIntegration {
    Codex,
    Claude,
    Opencode,
}

impl LaunchIntegration {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Debug, Args)]
pub struct LaunchArgs {
    #[arg(value_enum)]
    pub integration: LaunchIntegration,
    #[arg(long, env = "SY_SPARK_LAUNCH_MODEL")]
    pub model: Option<String>,
    #[arg(long = "config", env = "SY_SPARK_LAUNCH_CONFIG")]
    pub configure: bool,
    #[arg(long, env = "SY_SPARK_LAUNCH_RESTORE")]
    pub restore: bool,
    #[arg(long, short = 'y', env = "SY_SPARK_YES")]
    pub yes: bool,
    #[arg(long, env = "SY_SPARK_DRY_RUN")]
    pub dry_run: bool,
    #[arg(long, env = "SY_SPARK_JSON")]
    pub json: bool,
    #[arg(long, env = "SY_SPARK_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,
    #[arg(last = true)]
    pub extra_args: Vec<String>,
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
    pub release_manifest: PathBuf,
    #[arg(long)]
    pub models: PathBuf,
    #[arg(long = "engine", required = true)]
    pub engines: Vec<String>,
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
    /// Signed SHA256SUMS inventory beside the separate release payload files.
    #[arg(long, value_name = "PATH", env = "SY_SPARK_RELEASE_MANIFEST")]
    pub release_manifest: Option<PathBuf>,
    /// Explicit LAN address for the HTTPS listener.
    #[arg(long, value_name = "IP", env = "SY_SPARK_LISTEN_ADDRESS")]
    pub listen_address: Option<String>,
    /// HTTPS listener port (default 9843).
    #[arg(long, value_name = "PORT", env = "SY_SPARK_LISTEN_PORT")]
    pub listen_port: Option<u16>,
    /// Minisign signature for the release SHA256SUMS inventory (required with --yes).
    #[arg(long, value_name = "PATH", env = "SY_SPARK_RELEASE_SIGNATURE")]
    pub release_signature: Option<PathBuf>,
    /// Pinned minisign public key for the signed inventory (required with --yes).
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
            release_manifest: None,
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
    release_manifest: PathBuf,
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
        .unwrap_or_else(|| default_probe(env));
    let release_manifest = args
        .release_manifest
        .clone()
        .or_else(|| env.get("SY_SPARK_RELEASE_MANIFEST").map(PathBuf::from))
        .unwrap_or_else(|| probe.with_file_name("SHA256SUMS"));
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
        release_manifest,
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
        SparkCommand::Download(args) => dispatch_download(&cli.host, args),
        SparkCommand::Serve(args) => dispatch_serve(&cli.host, args),
        SparkCommand::Launch(args) => dispatch_launch(&cli.host, args),
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

fn dispatch_launch(host: &str, mut args: LaunchArgs) -> Result<(), SparkError> {
    let config_dir = args.config_dir.take().unwrap_or_else(default_config_dir);
    super::launch::run(host, &config_dir, args).map_err(SparkError::from_client)
}

fn dispatch_download(host: &str, args: DownloadArgs) -> Result<(), SparkError> {
    let request = DownloadRequest {
        repository: args.repository,
        revision: args.revision,
        alias: args.alias,
        artifact: args.artifact,
        auxiliary: args.auxiliary,
        update_alias: args.update_alias,
        dry_run: args.dry_run,
    };
    let client = load_client(host, args.config_dir)?;
    let key = idempotency_key(args.idempotency_key);
    if args.dry_run {
        let plan = client
            .download_plan(&key, &request)
            .map_err(SparkError::from_client)?;
        if args.json {
            print!("{}", render_json(&plan)?);
        } else {
            print!("{}", render_download_plan_human(&plan)?);
        }
        return Ok(());
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
    let client = load_client(host, args.config_dir)?;
    let key = idempotency_key(args.idempotency_key);
    if args.dry_run {
        let report = client
            .admission_plan(
                &key,
                &ServeAdmissionRequest {
                    model: args.model,
                    name: args.name,
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
    let document = active_instances(client.instances().map_err(SparkError::from_client)?);
    if args.json {
        print!("{}", render_json(&document)?);
    } else {
        print!("{}", render_instance_list_human(&document));
    }
    Ok(())
}

fn active_instances(
    mut document: super::wire::InstanceListDocument,
) -> super::wire::InstanceListDocument {
    use super::wire::InstanceObservedState::{Absent, Failed};
    document
        .instances
        .retain(|instance| !matches!(instance.observed, Absent | Failed));
    document
}

fn render_table<const COLUMNS: usize>(
    headers: [&str; COLUMNS],
    rows: &[[String; COLUMNS]],
) -> String {
    let widths = std::array::from_fn(|column| {
        rows.iter()
            .map(|row| row[column].chars().count())
            .max()
            .unwrap_or(0)
            .max(headers[column].chars().count())
    });
    let mut output = String::new();
    append_table_row(&mut output, &headers, &widths);
    for row in rows {
        let cells = std::array::from_fn(|column| row[column].as_str());
        append_table_row(&mut output, &cells, &widths);
    }
    output
}

fn append_table_row<const COLUMNS: usize>(
    output: &mut String,
    cells: &[&str; COLUMNS],
    widths: &[usize; COLUMNS],
) {
    for (column, cell) in cells.iter().enumerate() {
        if column > 0 {
            output.push_str("  ");
        }
        output.push_str(cell);
        if column + 1 < COLUMNS {
            output.extend(std::iter::repeat_n(
                ' ',
                widths[column].saturating_sub(cell.chars().count()),
            ));
        }
    }
    output.push('\n');
}

fn render_instance_list_human(document: &super::wire::InstanceListDocument) -> String {
    let rows = document
        .instances
        .iter()
        .map(|instance| {
            let model = instance
                .artifacts
                .configured_alias
                .as_deref()
                .unwrap_or_else(|| {
                    instance
                        .model
                        .strip_prefix("huggingface:")
                        .unwrap_or(&instance.model)
                        .split_once('@')
                        .map_or(instance.model.as_str(), |(repository, _)| repository)
                });
            [
                instance.name.clone(),
                model.to_owned(),
                instance.engine_id.clone(),
                match instance.context_window {
                    0 => "-".into(),
                    tokens if tokens % 1024 == 0 => format!("{}K", tokens / 1024),
                    tokens => tokens.to_string(),
                },
                format!("{:?}", instance.observed).to_ascii_lowercase(),
            ]
        })
        .collect::<Vec<_>>();
    render_table(["NAME", "MODEL", "ENGINE", "CONTEXT", "STATE"], &rows)
}

fn render_instance_human(instance: &super::wire::InstanceDocument) -> String {
    format!(
        "{}  {}  desired={:?} observed={:?} healthy={}\n  engine: {}\n  engine fingerprint: {}\n  artifact: {} ({:?}, {})\n  artifact fingerprint: {}\n",
        instance.name, instance.model, instance.desired, instance.observed, instance.healthy,
        instance.engine_id, instance.engine_fingerprint, instance.artifacts.primary.path,
        instance.artifacts.format, instance.artifacts.quantization.as_deref().unwrap_or("unknown"),
        instance.artifact_fingerprint,
    )
}

#[derive(Serialize)]
struct OperatorLogDocument<'a> {
    schema: &'static str,
    instance_id: &'a str,
    generation: u64,
    engine_id: &'a str,
    engine_fingerprint: &'a str,
    artifacts: &'a super::wire::ModelArtifactsDocument,
    artifact_fingerprint: &'a str,
    cursor: u64,
    next_cursor: u64,
    truncated: bool,
    lines: &'a [String],
}

fn operator_logs<'a>(
    logs: &'a super::wire::EngineLogDocument,
    instance: &'a super::wire::InstanceDocument,
) -> OperatorLogDocument<'a> {
    OperatorLogDocument {
        schema: "sy.spark.engine-logs/v2",
        instance_id: &logs.instance_id,
        generation: logs.generation,
        engine_id: &instance.engine_id,
        engine_fingerprint: &instance.engine_fingerprint,
        artifacts: &instance.artifacts,
        artifact_fingerprint: &instance.artifact_fingerprint,
        cursor: logs.cursor,
        next_cursor: logs.next_cursor,
        truncated: logs.truncated,
        lines: &logs.lines,
    }
}

fn render_logs_json(
    logs: &super::wire::EngineLogDocument,
    instance: &super::wire::InstanceDocument,
) -> Result<String, SparkError> {
    render_json(&operator_logs(logs, instance))
}

fn render_logs_human(
    logs: &super::wire::EngineLogDocument,
    instance: &super::wire::InstanceDocument,
    include_identity: bool,
) -> String {
    let mut output = if include_identity {
        render_instance_human(instance)
    } else {
        String::new()
    };
    for line in &logs.lines {
        output.push_str(line);
        output.push('\n');
    }
    output
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
    let instances = client.instances().map_err(SparkError::from_client)?;
    let instance = instances
        .instances
        .iter()
        .find(|instance| instance.id == args.instance || instance.name == args.instance)
        .ok_or_else(|| SparkError::usage("Spark instance was not found"))?;
    let mut cursor = args.cursor;
    let mut include_identity = true;
    loop {
        let document = client
            .instance_logs(&args.instance, cursor, args.limit)
            .map_err(SparkError::from_client)?;
        if !args.follow || !document.lines.is_empty() {
            if args.json {
                print!("{}", render_logs_json(&document, instance)?);
            } else {
                print!(
                    "{}",
                    render_logs_human(&document, instance, include_identity)
                );
                include_identity = false;
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
        print!("{}", render_model_list_json(&models)?);
    } else {
        print!("{}", render_model_list_human(&models)?);
    }
    Ok(())
}

fn dispatch_show(host: &str, args: ModelReadArgs) -> Result<(), SparkError> {
    let client = load_client(host, args.config_dir)?;
    let models = client.list_models().map_err(SparkError::from_client)?;
    let model = resolve_model(&models, &args.model)?;
    let model = client.model(&model.id).map_err(SparkError::from_client)?;
    if args.json {
        print!("{}", render_model_json(&model)?);
    } else {
        print!("{}", render_model_human(&model)?);
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

fn render_model_list_human(document: &ModelListDocument) -> Result<String, SparkError> {
    let rows = document
        .models
        .iter()
        .map(|model| {
            let id = model.id.strip_prefix("m_").unwrap_or(&model.id);
            [
                model.aliases.first().unwrap_or(&model.repository).clone(),
                id[..id.len().min(12)].to_owned(),
                crate::disk::human_bytes(model.logical_bytes),
                model.verified_at.get(..10).unwrap_or("-").to_owned(),
            ]
        })
        .collect::<Vec<_>>();
    Ok(render_table(["NAME", "ID", "SIZE", "MODIFIED"], &rows))
}

#[derive(Serialize)]
struct OperatorModelDocument<'a> {
    schema: &'static str,
    id: &'a str,
    canonical: &'a str,
    repository: &'a str,
    commit: &'a str,
    artifacts: &'a super::wire::ModelArtifactsDocument,
    artifact_fingerprint: String,
    logical_bytes: u64,
    unique_bytes: u64,
    aliases: &'a [String],
    active_instances: &'a [String],
    transport: &'a str,
    verified_at: &'a str,
    gated: bool,
    license: &'a Option<String>,
}

fn operator_model(model: &ModelDocument) -> Result<OperatorModelDocument<'_>, SparkError> {
    let artifacts = model
        .artifacts
        .as_ref()
        .ok_or_else(|| SparkError::usage("model artifact identity is missing"))?;
    Ok(OperatorModelDocument {
        schema: "sy.spark.model/v2",
        id: &model.id,
        canonical: &model.canonical,
        repository: &model.repository,
        commit: &model.commit,
        artifacts,
        artifact_fingerprint: super::wire::artifact_fingerprint(artifacts)
            .map_err(SparkError::usage)?,
        logical_bytes: model.logical_bytes,
        unique_bytes: model.unique_bytes,
        aliases: &model.aliases,
        active_instances: &model.active_instances,
        transport: &model.transport,
        verified_at: &model.verified_at,
        gated: model.gated,
        license: &model.license,
    })
}

fn render_model_json(model: &ModelDocument) -> Result<String, SparkError> {
    render_json(&operator_model(model)?)
}

#[derive(Serialize)]
struct OperatorModelListDocument<'a> {
    schema: &'static str,
    models: Vec<OperatorModelDocument<'a>>,
}

fn render_model_list_json(document: &ModelListDocument) -> Result<String, SparkError> {
    let models = document
        .models
        .iter()
        .map(operator_model)
        .collect::<Result<Vec<_>, _>>()?;
    render_json(&OperatorModelListDocument {
        schema: "sy.spark.model-list/v2",
        models,
    })
}

fn render_download_plan_human(
    plan: &super::wire::DownloadPlanDocument,
) -> Result<String, SparkError> {
    let artifacts = plan
        .artifacts
        .as_ref()
        .ok_or_else(|| SparkError::usage("download plan artifact identity is missing"))?;
    let fingerprint = super::wire::artifact_fingerprint(artifacts).map_err(SparkError::usage)?;
    Ok(format!(
        "{}@{}\nartifact: {} ({:?}, {})\nartifact fingerprint: {}\nlogical bytes: {}\nunique bytes: {}\ntemporary bytes: {}\ndisk reserve bytes: {}\n",
        plan.repository, plan.commit, artifacts.primary.path, artifacts.format,
        artifacts.quantization.as_deref().unwrap_or("unknown"), fingerprint,
        plan.logical_bytes, plan.unique_bytes, plan.temporary_bytes, plan.disk_reserve_bytes,
    ))
}

fn render_model_human(model: &ModelDocument) -> Result<String, SparkError> {
    let artifacts = model
        .artifacts
        .as_ref()
        .ok_or_else(|| SparkError::usage("model artifact identity is missing"))?;
    let fingerprint = super::wire::artifact_fingerprint(artifacts).map_err(SparkError::usage)?;
    let mut output = format!(
        "{}\nartifact: {} ({:?}, {})\nartifact fingerprint: {}\nlogical bytes: {}\nunique bytes: {}\ntransport: {}\n",
        model.canonical, artifacts.primary.path, artifacts.format,
        artifacts.quantization.as_deref().unwrap_or("unknown"), fingerprint,
        model.logical_bytes, model.unique_bytes, model.transport,
    );
    if !model.aliases.is_empty() {
        output.push_str(&format!("aliases: {}\n", model.aliases.join(",")));
    }
    if !model.active_instances.is_empty() {
        output.push_str(&format!("active: {}\n", model.active_instances.join(",")));
    }
    Ok(output)
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
            "  policy: {}\n  engine id: {}\n  engine: {}\n  image: {}\n  fingerprint: {}\n  artifact: {} ({:?})\n  artifact fingerprint: {}\n",
            selection.selection_kind,
            selection.engine_id,
            selection.engine,
            selection.image,
            selection.fingerprint,
            selection.artifacts.primary.path,
            selection.artifacts.format,
            selection.artifact_fingerprint,
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
    let engines = args
        .engines
        .iter()
        .map(|value| install::parse_staged_engine_asset(value))
        .collect::<Result<Vec<_>, _>>()
        .map_err(SparkError::from_install)?;
    let (report, material) = install::activate_from_files(&install::ActivateRequest {
        root: PathBuf::from("/"),
        executable: args.executable,
        signature: args.signature,
        public_key: args.public_key,
        manifest: args.manifest,
        manifest_sha256: args.manifest_sha256,
        release_manifest: args.release_manifest,
        models: args.models,
        engines,
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
    let payload = install::load_release_payload(&resolved.release_manifest, &resolved.probe)
        .map_err(SparkError::from_install)?;
    let mut manifest = install::inspect_and_plan(
        &OpenSshRunner,
        InstallRequest {
            host_alias: host,
            probe_path: resolved.probe,
            listen_address: resolved.listen_address,
            listen_port: resolved.listen_port,
            catalogs: payload.catalogs.clone(),
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
            release_manifest: &payload.manifest,
            models: &payload.models,
            engines: &payload.engines,
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

    fn test_artifacts() -> crate::spark::wire::ModelArtifactsDocument {
        serde_json::from_str(concat!(
            r#"{"schema":"sy.spark.model-artifacts/v2","format":"gguf","primary":{"path":"model.gguf","bytes":8,"sha256":"#,
            r#""aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"auxiliary":[],"quantization":"Q4_K_XL","capabilities":["text_generation"],"configured_alias":"model:q4"}"#,
        ))
        .unwrap()
    }

    fn test_model(
        artifacts: crate::spark::wire::ModelArtifactsDocument,
    ) -> crate::spark::wire::ModelDocument {
        crate::spark::wire::ModelDocument {
            schema: "sy.spark.model/v1".into(),
            id: "model-id".into(),
            canonical: "owner/model@commit".into(),
            repository: "owner/model".into(),
            commit: "c".repeat(40),
            snapshot: "redacted".into(),
            artifacts: Some(artifacts),
            logical_bytes: 8,
            unique_bytes: 8,
            aliases: vec!["model:q4".into()],
            active_instances: vec!["model".into()],
            transport: "hf-hub".into(),
            verified_at: "2026-08-27T00:00:00Z".into(),
            gated: false,
            license: None,
        }
    }

    fn test_instance(
        artifacts: crate::spark::wire::ModelArtifactsDocument,
        artifact_fingerprint: String,
    ) -> crate::spark::wire::InstanceDocument {
        use crate::spark::wire::{
            InstanceDesiredState, InstanceObservedState, RecipeResourceEnvelopeDocument,
        };
        crate::spark::wire::InstanceDocument {
            schema: "sy.spark.instance/v2".into(),
            id: "instance-id".into(),
            name: "model".into(),
            model_id: "model-id".into(),
            model: "owner/model@commit".into(),
            model_commit: "c".repeat(40),
            engine_id: "llama-cpp-cuda13-arm64".into(),
            engine_fingerprint: format!("sha256:{}", "b".repeat(64)),
            artifacts,
            artifact_fingerprint,
            objective: "chat".into(),
            resources: RecipeResourceEnvelopeDocument {
                image_bytes: 1,
                startup_peak_bytes: 2,
                steady_peak_bytes: 3,
                compile_cache_bytes: 4,
            },
            context_window: 65_536,
            default_reasoning_effort: None,
            generation: 1,
            desired: InstanceDesiredState::Running,
            observed: InstanceObservedState::Healthy,
            endpoint: Some("https://spark/openai/model/v1".into()),
            healthy: true,
            started_at: None,
            startup_milliseconds: Some(1),
            last_failure: None,
            restart_failures: 0,
            restart_suppressed: false,
            quarantine: None,
        }
    }

    #[test]
    fn model_and_process_render_engine_artifact_identity() {
        let artifacts = test_artifacts();
        let artifact_fingerprint = crate::spark::wire::artifact_fingerprint(&artifacts).unwrap();
        let model = test_model(artifacts.clone());
        let instance = test_instance(artifacts, artifact_fingerprint.clone());
        let output = format!(
            "{}{}",
            super::render_model_human(&model).unwrap(),
            super::render_instance_human(&instance)
        );
        assert!(output.contains(&format!("artifact fingerprint: {artifact_fingerprint}")));
        assert!(output.contains("engine fingerprint: sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
    }

    #[test]
    fn model_list_is_one_compact_ollama_style_table() {
        let model = test_model(test_artifacts());
        let document = crate::spark::wire::ModelListDocument {
            schema: "sy.spark.model-list/v1".into(),
            models: vec![model],
        };

        assert_eq!(
            super::render_model_list_human(&document).unwrap(),
            "NAME      ID        SIZE  MODIFIED\nmodel:q4  model-id  8B    2026-08-27\n"
        );
    }

    #[test]
    fn model_list_uses_repository_when_alias_is_absent() {
        let mut model = test_model(test_artifacts());
        model.aliases.clear();
        let document = crate::spark::wire::ModelListDocument {
            schema: "sy.spark.model-list/v1".into(),
            models: vec![model],
        };

        assert!(super::render_model_list_human(&document)
            .unwrap()
            .contains("owner/model  model-id"));
    }

    #[test]
    fn process_list_contains_only_active_lifecycle_instances() {
        use crate::spark::wire::InstanceObservedState;
        let artifacts = test_artifacts();
        let fingerprint = crate::spark::wire::artifact_fingerprint(&artifacts).unwrap();
        let healthy = test_instance(artifacts.clone(), fingerprint.clone());
        let mut warming = test_instance(artifacts.clone(), fingerprint.clone());
        warming.name = "warming".into();
        warming.observed = InstanceObservedState::Warming;
        let mut stopping = test_instance(artifacts.clone(), fingerprint.clone());
        stopping.name = "stopping".into();
        stopping.observed = InstanceObservedState::Stopping;
        let mut absent = test_instance(artifacts.clone(), fingerprint.clone());
        absent.name = "absent".into();
        absent.observed = InstanceObservedState::Absent;
        let mut failed = test_instance(artifacts, fingerprint);
        failed.name = "failed".into();
        failed.observed = InstanceObservedState::Failed;

        let filtered = super::active_instances(crate::spark::wire::InstanceListDocument {
            schema: "sy.spark.instance-list/v1".into(),
            instances: vec![healthy, warming, stopping, absent, failed],
        });

        assert_eq!(
            filtered
                .instances
                .iter()
                .map(|instance| instance.name.as_str())
                .collect::<Vec<_>>(),
            ["model", "warming", "stopping"]
        );
        assert_eq!(
            serde_json::to_value(filtered).unwrap()["instances"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn process_list_is_one_compact_ollama_style_table() {
        let artifacts = test_artifacts();
        let fingerprint = crate::spark::wire::artifact_fingerprint(&artifacts).unwrap();
        let document = crate::spark::wire::InstanceListDocument {
            schema: "sy.spark.instance-list/v1".into(),
            instances: vec![test_instance(artifacts, fingerprint)],
        };

        let output = super::render_instance_list_human(&document);

        assert!(output.starts_with("NAME   MODEL     ENGINE"));
        assert!(output.contains("model  model:q4  llama-cpp-cuda13-arm64  64K      healthy"));
        assert_eq!(output.lines().count(), 2);
        assert!(!output.contains("fingerprint"));
    }

    #[test]
    fn empty_model_and_process_lists_render_headers_only() {
        let models = crate::spark::wire::ModelListDocument {
            schema: "sy.spark.model-list/v1".into(),
            models: Vec::new(),
        };
        let instances = crate::spark::wire::InstanceListDocument {
            schema: "sy.spark.instance-list/v1".into(),
            instances: Vec::new(),
        };

        assert_eq!(
            super::render_model_list_human(&models).unwrap(),
            "NAME  ID  SIZE  MODIFIED\n"
        );
        assert_eq!(
            super::render_instance_list_human(&instances),
            "NAME  MODEL  ENGINE  CONTEXT  STATE\n"
        );
    }

    #[test]
    fn download_dry_run_renders_exact_artifact_identity() {
        let plan = crate::spark::wire::DownloadPlanDocument {
            schema: "sy.spark.download-plan/v1".into(),
            repository: "owner/model".into(),
            commit: "c".repeat(40),
            artifacts: Some(test_artifacts()),
            logical_bytes: 8,
            unique_bytes: 8,
            temporary_bytes: 8,
            disk_reserve_bytes: 8,
        };
        let output = super::render_download_plan_human(&plan).unwrap();
        assert!(output.contains("artifact fingerprint: sha256:"));
    }

    #[test]
    fn json_documents_remain_machine_readable() {
        let artifacts = test_artifacts();
        let fingerprint = crate::spark::wire::artifact_fingerprint(&artifacts).unwrap();
        let model = test_model(artifacts.clone());
        let model_json: serde_json::Value =
            serde_json::from_str(&super::render_model_json(&model).unwrap()).unwrap();
        assert_eq!(model_json["artifact_fingerprint"], fingerprint);
        assert!(model_json.get("snapshot").is_none());
        let instance = test_instance(artifacts, fingerprint.clone());
        let logs = crate::spark::wire::EngineLogDocument {
            schema: "sy.spark.engine-logs/v1".into(),
            instance_id: instance.id.clone(),
            generation: 1,
            cursor: 0,
            next_cursor: 1,
            truncated: false,
            lines: vec!["ready".into()],
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&super::render_logs_json(&logs, &instance).unwrap()).unwrap();
        assert_eq!(parsed["artifact_fingerprint"], fingerprint);
    }

    #[test]
    fn model_command_help_documents_examples_and_environment() {
        #[derive(Debug, clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        for command in [
            "download", "serve", "launch", "ps", "logs", "stop", "ls", "show", "rm",
        ] {
            let error =
                TestCli::try_parse_from(["sy", "dgx-spark", command, "--help"]).unwrap_err();
            let help = error.to_string();
            assert!(help.contains("Example"), "{command} help lacks examples");
            assert!(
                help.contains("SY_SPARK_"),
                "{command} help lacks environment equivalents"
            );
            assert!(help.contains("--json"), "{command} help lacks --json");
            if ["download", "serve", "launch", "stop", "rm"].contains(&command) {
                assert!(help.contains("--dry-run"), "{command} help lacks --dry-run");
            }
        }
    }

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
    fn serve_rejects_recipe_and_unverified_selection_options() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let serve = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "serve",
            "ornith-1.5:9b",
            "--dry-run",
            "--json",
        ])
        .unwrap();
        assert!(matches!(serve.spark.command, super::SparkCommand::Serve(_)));
        for flag in ["--recipe", "--allow-unverified"] {
            assert!(TestCli::try_parse_from([
                "sy",
                "dgx-spark",
                "serve",
                "ornith-1.5:9b",
                flag,
                "value",
            ])
            .is_err());
        }
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
            release_manifest: Some(PathBuf::from("/flag/SHA256SUMS")),
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
                "\"release_manifest\": \"/flag/SHA256SUMS\",\n  ",
                "\"listen_address\": \"10.1.30.143\",\n  \"listen_port\": 9443,\n  \"release_signature\": null,\n  \"release_public_key\": null,\n  \"config_dir\": ",
                "\"/flag/config\"\n}\n"
            )
        );

        let defaults =
            resolve_install(&InstallArgs::dry_run_for_test(), &MapEnv::default()).unwrap();
        assert_eq!(
            defaults.probe,
            PathBuf::from("/usr/local/share/sy/spark-release/sy-aarch64")
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
                catalogs: crate::spark::install::CatalogDigests {
                    models: "m".into(),
                    engines: std::collections::BTreeMap::from([
                        ("configs/sy/spark/engines/first.toml".into(), "l".into()),
                        ("configs/sy/spark/engines/second.toml".into(), "v".into()),
                    ]),
                },
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
                engine_id: "vllm-arm64".into(),
                selection_kind: "configured_engine".into(),
                engine: "vllm".into(),
                image: IMAGE.into(),
                fingerprint: FINGERPRINT.into(),
                artifacts: serde_json::from_str(r#"{"schema":"sy.spark.model-artifacts/v2","format":"safetensors","primary":{"path":"model.safetensors","bytes":8,"sha256":null},"auxiliary":[],"quantization":"FP8","capabilities":["text_generation"],"configured_alias":null}"#).unwrap(),
                artifact_fingerprint: format!("sha256:{}", "c".repeat(64)),
                compile_cache_namespace: "opaque-cache-namespace".into(),
            }),
        };

        assert_eq!(
            super::render_admission_human(&report),
            format!(
                "Spark admission: admitted (aggregate 42 bytes; reserve 8 bytes)\n  policy: configured_engine\n  engine id: vllm-arm64\n  engine: vllm\n  image: {IMAGE}\n  fingerprint: {FINGERPRINT}\n  artifact: model.safetensors (Safetensors)\n  artifact fingerprint: sha256:{}\n",
                "c".repeat(64)
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
    fn download_artifact_flags_are_agent_friendly() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let parsed = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "download",
            "owner/model",
            "--artifact",
            "weights/model.gguf",
            "--auxiliary",
            "projector=vision/mmproj.gguf",
            "--auxiliary",
            "weight_shard=model-00002.gguf",
        ])
        .unwrap();
        let super::SparkCommand::Download(args) = parsed.spark.command else {
            panic!("download command expected")
        };
        assert_eq!(args.artifact.as_deref(), Some("weights/model.gguf"));
        assert_eq!(args.auxiliary[0].path, "vision/mmproj.gguf");
        assert_eq!(args.auxiliary[1].path, "model-00002.gguf");
    }

    #[test]
    fn download_rejects_unlabelled_auxiliary_artifacts() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        assert!(TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "download",
            "owner/model",
            "--artifact",
            "model.gguf",
            "--auxiliary",
            "mmproj.gguf",
        ])
        .is_err());
    }

    #[test]
    fn launch_parses_model_and_exact_agent_arguments() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            spark: super::SparkCli,
        }
        let parsed = TestCli::try_parse_from([
            "sy",
            "dgx-spark",
            "launch",
            "codex",
            "--model",
            "ornith-1.5:9b",
            "--",
            "--sandbox",
            "workspace-write",
        ])
        .unwrap();
        let super::SparkCommand::Launch(args) = parsed.spark.command else {
            panic!("launch command should parse");
        };
        assert_eq!(args.extra_args, ["--sandbox", "workspace-write"]);
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
