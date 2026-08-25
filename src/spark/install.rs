//! Typed OpenSSH bootstrap and pure installation-plan construction.

use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use super::wire::{
    decode_inventory, Applicability, AssetKind, ContentIdentity, ExecutionPhase, HostInventory,
    InstallExecution, InstallManifest, MigrationPlan, PlannedAsset, ProbeEvidence,
    ProtectedFingerprint, RejectedUpdateClass, RollbackPlan, ServiceTransition, MANIFEST_SCHEMA,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeTransfer {
    pub host_alias: String,
    pub local_path: PathBuf,
    pub remote_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectInvocation {
    host_alias: String,
    remote_path: String,
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivateRequest {
    pub root: PathBuf,
    pub executable: PathBuf,
    pub signature: PathBuf,
    pub public_key: PathBuf,
    pub manifest: PathBuf,
    pub manifest_sha256: String,
    pub version: String,
    pub listen_address: String,
    pub hostname: String,
    pub active_lsm: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerErrorKind {
    Unreachable,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerError {
    pub kind: RunnerErrorKind,
    pub message: String,
}

impl fmt::Display for RunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RunnerError {}

pub trait BootstrapRunner: Send + Sync {
    fn upload_probe(&self, transfer: &ProbeTransfer) -> Result<(), RunnerError>;
    fn inspect(&self, invocation: &InspectInvocation) -> Result<Vec<u8>, RunnerError>;
    fn remove_probe(&self, transfer: &ProbeTransfer) -> Result<(), RunnerError>;
}

impl InspectInvocation {
    pub fn from_transfer(transfer: &ProbeTransfer) -> Self {
        Self {
            host_alias: transfer.host_alias.clone(),
            remote_path: transfer.remote_path.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub struct OpenSshRunner;

impl OpenSshRunner {
    fn upload_process(transfer: &ProbeTransfer) -> Result<Command, String> {
        validate_remote_probe_path(&transfer.remote_path)?;
        let mut command = Command::new("sftp");
        command.args(["-o", "BatchMode=no", "-b", "-", "--", &transfer.host_alias]);
        Ok(command)
    }

    fn inspect_process(invocation: &InspectInvocation) -> Command {
        let mut command = Command::new("ssh");
        command
            .arg("--")
            .arg(&invocation.host_alias)
            .arg(&invocation.remote_path)
            .args(["spark", "bootstrap", "inspect"]);
        command
    }

    fn sftp_batch(transfer: &ProbeTransfer, remove: bool) -> Result<String, RunnerError> {
        validate_remote_probe_path(&transfer.remote_path).map_err(protocol_error)?;
        if remove {
            return Ok(format!("-rm {}\n", transfer.remote_path));
        }
        let local = transfer
            .local_path
            .to_str()
            .ok_or_else(|| protocol_error("bootstrap probe path is not UTF-8"))?;
        if local.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
            return Err(protocol_error(
                "bootstrap probe path contains an unsupported control character",
            ));
        }
        Ok(format!(
            "put \"{}\" {}\nchmod 0700 {}\n",
            local.replace('\\', "\\\\").replace('"', "\\\""),
            transfer.remote_path,
            transfer.remote_path
        ))
    }

    fn run_sftp(&self, transfer: &ProbeTransfer, remove: bool) -> Result<(), RunnerError> {
        let batch = Self::sftp_batch(transfer, remove)?;
        let mut process = Self::upload_process(transfer)
            .map_err(protocol_error)?
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|error| unreachable_error(format!("start sftp: {error}")))?;
        process
            .stdin
            .take()
            .ok_or_else(|| protocol_error("sftp stdin was unavailable"))?
            .write_all(batch.as_bytes())
            .map_err(|error| unreachable_error(format!("write sftp batch: {error}")))?;
        let status = process
            .wait()
            .map_err(|error| unreachable_error(format!("wait for sftp: {error}")))?;
        if !status.success() {
            return Err(unreachable_error(format!("sftp exited with {status}")));
        }
        Ok(())
    }
}

impl BootstrapRunner for OpenSshRunner {
    fn upload_probe(&self, transfer: &ProbeTransfer) -> Result<(), RunnerError> {
        self.run_sftp(transfer, false)
    }

    fn inspect(&self, invocation: &InspectInvocation) -> Result<Vec<u8>, RunnerError> {
        let output = Self::inspect_process(invocation)
            .output()
            .map_err(|error| unreachable_error(format!("start ssh: {error}")))?;
        if !output.status.success() {
            return Err(unreachable_error(format!(
                "ssh bootstrap inspect exited with {}",
                output.status
            )));
        }
        Ok(output.stdout)
    }

    fn remove_probe(&self, transfer: &ProbeTransfer) -> Result<(), RunnerError> {
        self.run_sftp(transfer, true)
    }
}

fn protocol_error(message: impl Into<String>) -> RunnerError {
    RunnerError {
        kind: RunnerErrorKind::Protocol,
        message: message.into(),
    }
}

fn unreachable_error(message: impl Into<String>) -> RunnerError {
    RunnerError {
        kind: RunnerErrorKind::Unreachable,
        message: message.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOptions {
    pub host_alias: String,
    pub listen_address: String,
    pub listen_port: u16,
    pub probe_remote_path: String,
    pub probe_sha256: String,
    pub probe_removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub host_alias: String,
    pub probe_path: PathBuf,
    pub listen_address: Option<String>,
    pub listen_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallErrorKind {
    Configuration,
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallError {
    pub kind: InstallErrorKind,
    pub message: String,
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InstallError {}

pub fn inspect_and_plan(
    runner: &dyn BootstrapRunner,
    request: InstallRequest,
) -> Result<InstallManifest, InstallError> {
    let probe_sha256 = sha256_file(&request.probe_path)?;
    let remote_path = format!("/tmp/sy-spark-bootstrap-{probe_sha256}");
    let transfer = ProbeTransfer {
        host_alias: request.host_alias.clone(),
        local_path: request.probe_path,
        remote_path: remote_path.clone(),
    };
    if let Err(upload_error) = runner.upload_probe(&transfer) {
        let cleanup = runner.remove_probe(&transfer);
        let mut error = map_runner_error(upload_error);
        if let Err(cleanup_error) = cleanup {
            error.message = format!(
                "{}; exact remote probe cleanup also failed: {}",
                error.message, cleanup_error
            );
        }
        return Err(error);
    }
    let inspection = runner.inspect(&InspectInvocation::from_transfer(&transfer));
    let cleanup = runner.remove_probe(&transfer);
    if let Err(error) = cleanup {
        return Err(InstallError {
            kind: InstallErrorKind::Unreachable,
            message: format!("failed to remove exact remote probe path {remote_path}: {error}"),
        });
    }
    let inventory_bytes = inspection.map_err(map_runner_error)?;
    let inventory = decode_inventory(&inventory_bytes).map_err(configuration_error)?;
    let listen_address = request
        .listen_address
        .or_else(|| only_address(&inventory.lan_addresses))
        .ok_or_else(|| {
            configuration_error(
                "set --listen-address: discovery did not return exactly one LAN address",
            )
        })?;
    build_manifest(
        inventory,
        PlanOptions {
            host_alias: request.host_alias,
            listen_address,
            listen_port: request.listen_port,
            probe_remote_path: remote_path,
            probe_sha256,
            probe_removed: true,
        },
    )
    .map_err(configuration_error)
}

fn only_address(addresses: &[String]) -> Option<String> {
    match addresses {
        [address] => Some(address.clone()),
        _ => None,
    }
}

fn sha256_file(path: &Path) -> Result<String, InstallError> {
    let mut file = File::open(path).map_err(|error| {
        configuration_error(format!(
            "open ARM64 bootstrap probe {}: {error}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            configuration_error(format!(
                "read ARM64 bootstrap probe {}: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn map_runner_error(error: RunnerError) -> InstallError {
    let kind = match error.kind {
        RunnerErrorKind::Unreachable => InstallErrorKind::Unreachable,
        RunnerErrorKind::Protocol => InstallErrorKind::Configuration,
    };
    InstallError {
        kind,
        message: error.message,
    }
}

fn configuration_error(message: impl Into<String>) -> InstallError {
    InstallError {
        kind: InstallErrorKind::Configuration,
        message: message.into(),
    }
}

pub fn build_manifest(
    inventory: HostInventory,
    options: PlanOptions,
) -> Result<InstallManifest, String> {
    validate_remote_probe_path(&options.probe_remote_path)?;
    if inventory.probe_sha256 != options.probe_sha256 {
        return Err("remote bootstrap probe hash differs from the uploaded artifact".into());
    }
    if !options.probe_removed {
        return Err("remote bootstrap probe cleanup was not confirmed".into());
    }
    if !inventory.lan_addresses.contains(&options.listen_address) {
        return Err(format!(
            "listen address {} is not an inspected LAN address",
            options.listen_address
        ));
    }
    let protected_before = ProtectedFingerprint::from_inventory(&inventory)?;
    let assets = planned_assets(&inventory, &options.probe_sha256)?;
    let mut manifest = InstallManifest {
        schema: MANIFEST_SCHEMA.into(),
        operation: "install".into(),
        dry_run: true,
        installation_performed: false,
        host_alias: options.host_alias,
        listen_address: options.listen_address,
        listen_port: options.listen_port,
        probe: ProbeEvidence {
            local_sha256: options.probe_sha256,
            reported_sha256: inventory.probe_sha256.clone(),
            remote_path: options.probe_remote_path,
            removed: options.probe_removed,
        },
        protected_before,
        protected_versions_must_remain_unchanged: true,
        rejected_updates: rejected_updates(),
        service_transitions: planned_service_transitions(),
        migration: MigrationPlan {
            database: "/var/lib/sy-spark/state.sqlite3".into(),
            from_schema: inventory.existing_installation.state_schema.clone(),
            to_schema: "sy.spark.state/v1".into(),
            backup_before_migration: inventory.existing_installation.present,
            applicability: deferred(3),
        },
        rollback: RollbackPlan {
            activation_path: "/opt/sy-spark/current".into(),
            target_release: inventory.existing_installation.current_release.clone(),
            retain_preceding_release: true,
            restore_database_backup: inventory.existing_installation.present,
        },
        inventory,
        assets,
        approval_sha256: String::new(),
        execution: InstallExecution::Planned,
    };
    manifest.approval_sha256 = manifest_approval_sha256(&manifest)?;
    Ok(manifest)
}

pub fn manifest_approval_sha256(manifest: &InstallManifest) -> Result<String, String> {
    let mut planned = manifest.clone();
    planned.approval_sha256.clear();
    planned.dry_run = true;
    planned.installation_performed = false;
    planned.execution = InstallExecution::Planned;
    serde_json::to_vec(&planned)
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| format!("encode canonical Spark approval manifest: {error}"))
}

fn planned_assets(
    inventory: &HostInventory,
    executable_sha: &str,
) -> Result<Vec<PlannedAsset>, String> {
    let mut assets = Vec::new();
    let fallback_digest = format!(
        "{:x}",
        Sha256::digest(include_bytes!(
            "../../configs/sy/spark/hf-http-fallback.lock"
        ))
    );
    let release_path = format!(
        "/opt/sy-spark/releases/{}",
        release_component(env!("CARGO_PKG_VERSION"), executable_sha)
    );
    for (path, owner, mode, applicability) in [
        ("/opt/sy-spark", "root:root", "0755", apply_remote()),
        (
            "/opt/sy-spark/releases",
            "root:root",
            "0755",
            apply_remote(),
        ),
        (&release_path, "root:root", "0755", apply_remote()),
        (
            "/opt/sy-spark/hf-http-fallback",
            "root:root",
            "0755",
            apply_remote(),
        ),
        ("/etc/sy", "root:sy-spark", "0750", apply_remote()),
        (
            "/etc/sy/spark-recipes.d",
            "root:root",
            "0755",
            apply_remote(),
        ),
        (
            "/var/lib/sy-spark",
            "sy-spark:sy-spark",
            "0750",
            apply_remote(),
        ),
        (
            "/var/lib/sy-spark/huggingface",
            "sy-spark:sy-spark",
            "0750",
            apply_remote(),
        ),
        (
            "/var/lib/sy-spark/compile-cache",
            "root:sy-spark",
            "0750",
            apply_remote(),
        ),
        (
            "/var/lib/sy-spark/tls",
            "sy-spark:sy-spark",
            "0700",
            apply_remote(),
        ),
        ("/var/lib/sy-spark/ca", "root:root", "0700", apply_remote()),
        (
            "/var/lib/sy-spark/executor",
            "root:sy-spark",
            "0750",
            apply_remote(),
        ),
        ("/run/sy-spark", "root:sy-spark", "0750", apply_remote()),
    ] {
        assets.push(asset(
            AssetKind::Directory,
            path,
            owner,
            mode,
            ContentIdentity::NotApplicable,
            applicability,
        ));
    }
    assets.extend([
        asset(
            AssetKind::Identity,
            "user:sy-spark",
            "root:root",
            "non-login",
            ContentIdentity::NotApplicable,
            apply_remote(),
        ),
        asset(
            AssetKind::Identity,
            "group:sy-spark",
            "root:root",
            "system",
            ContentIdentity::NotApplicable,
            apply_remote(),
        ),
        asset(
            AssetKind::File,
            &format!("{release_path}/sy"),
            "root:root",
            "0555",
            ContentIdentity::Sha256(executable_sha.into()),
            apply_remote(),
        ),
        asset(
            AssetKind::Symlink,
            "/opt/sy-spark/current",
            "root:root",
            "atomic",
            ContentIdentity::SignedReleaseManifest,
            apply_remote(),
        ),
        asset(
            AssetKind::File,
            &format!("/opt/sy-spark/hf-http-fallback/{fallback_digest}/requirements.lock"),
            "root:root",
            "0444",
            ContentIdentity::SignedReleaseManifest,
            apply_remote(),
        ),
        asset(
            AssetKind::Credential,
            "/etc/sy/spark-hf-read.credential",
            "root:root",
            "0600",
            ContentIdentity::SignedReleaseManifest,
            apply_remote(),
        ),
        asset(
            AssetKind::File,
            "/etc/sy/spark-agent.toml",
            "root:sy-spark",
            "0640",
            ContentIdentity::SignedReleaseManifest,
            apply_remote(),
        ),
        asset(
            AssetKind::File,
            "/etc/sy/spark-executor.toml",
            "root:sy-spark",
            "0640",
            ContentIdentity::SignedReleaseManifest,
            apply_remote(),
        ),
        asset(
            AssetKind::File,
            "/var/lib/sy-spark/state.sqlite3",
            "sy-spark:sy-spark",
            "0600",
            ContentIdentity::GeneratedAtInstall,
            deferred(3),
        ),
        asset(
            AssetKind::File,
            "/var/lib/sy-spark/executor/emergency.jsonl",
            "root:root",
            "0600",
            ContentIdentity::GeneratedAtInstall,
            deferred(3),
        ),
        asset(
            AssetKind::Certificate,
            "/var/lib/sy-spark/ca/ca-key.pem",
            "root:root",
            "0600",
            ContentIdentity::GeneratedAtInstall,
            apply_remote(),
        ),
        asset(
            AssetKind::Certificate,
            "/var/lib/sy-spark/ca/ca-cert.pem",
            "root:root",
            "0644",
            ContentIdentity::GeneratedAtInstall,
            apply_remote(),
        ),
        asset(
            AssetKind::Certificate,
            "/var/lib/sy-spark/tls/server-key.pem",
            "sy-spark:sy-spark",
            "0600",
            ContentIdentity::GeneratedAtInstall,
            apply_remote(),
        ),
        asset(
            AssetKind::Certificate,
            "/var/lib/sy-spark/tls/server-chain.pem",
            "sy-spark:sy-spark",
            "0644",
            ContentIdentity::GeneratedAtInstall,
            apply_remote(),
        ),
        asset(
            AssetKind::Credential,
            "/etc/sy/spark-bootstrap-admin.credential",
            "root:root",
            "0600",
            ContentIdentity::GeneratedAtInstall,
            apply_remote(),
        ),
        asset(
            AssetKind::Credential,
            "local-config:spark/<host>",
            "operator:operator",
            "0600",
            ContentIdentity::GeneratedAtInstall,
            Applicability::ApplyNow {
                phase: ExecutionPhase::LocalCredentialStore,
            },
        ),
    ]);
    if inventory.existing_installation.present {
        assets.push(asset(
            AssetKind::Symlink,
            "/opt/sy-spark/previous",
            "root:root",
            "atomic",
            ContentIdentity::SignedReleaseManifest,
            apply_remote(),
        ));
    }
    for (name, bytes) in [
        (
            "ornith-vllm.toml",
            include_bytes!("../../configs/sy/spark/recipes/ornith-vllm.toml").as_slice(),
        ),
        (
            "qwen3-embedding.toml",
            include_bytes!("../../configs/sy/spark/recipes/qwen3-embedding.toml").as_slice(),
        ),
        (
            "fixture-http-echo.toml",
            include_bytes!("../../configs/sy/spark/recipes/fixture-http-echo.toml").as_slice(),
        ),
    ] {
        assets.push(asset(
            AssetKind::Recipe,
            &format!("/etc/sy/spark-recipes.d/{name}"),
            "root:root",
            "0644",
            ContentIdentity::Sha256(format!("{:x}", Sha256::digest(bytes))),
            apply_remote(),
        ));
    }
    for (unit, applicability) in [
        ("/etc/systemd/system/sy-spark-agent.service", apply_remote()),
        (
            "/etc/systemd/system/sy-spark-executor.service",
            apply_remote(),
        ),
        ("/etc/systemd/system/sy-spark.target", apply_remote()),
    ] {
        assets.push(asset(
            AssetKind::SystemdUnit,
            unit,
            "root:root",
            "0644",
            ContentIdentity::SignedReleaseManifest,
            applicability,
        ));
    }
    match (inventory.lsm.kind.as_str(), inventory.lsm.mode.as_str()) {
        ("apparmor", "enforce") => assets.push(asset(
            AssetKind::LsmPolicy,
            "/etc/apparmor.d/sy-spark-agent",
            "root:root",
            "0644",
            ContentIdentity::SignedReleaseManifest,
            apply_remote(),
        )),
        ("none", "disabled") => {}
        (kind, mode) => {
            return Err(format!(
                "unsupported or unenforced active LSM: {kind}:{mode}"
            ))
        }
    }
    if inventory.lsm.kind == "apparmor" && inventory.lsm.mode == "enforce" {
        assets.push(asset(
            AssetKind::LsmPolicy,
            "/etc/apparmor.d/sy-spark-executor",
            "root:root",
            "0644",
            ContentIdentity::SignedReleaseManifest,
            apply_remote(),
        ));
    }
    Ok(assets)
}

fn asset(
    kind: AssetKind,
    path_or_name: &str,
    owner: &str,
    mode: &str,
    content: ContentIdentity,
    applicability: Applicability,
) -> PlannedAsset {
    PlannedAsset {
        kind,
        path_or_name: path_or_name.into(),
        owner: owner.into(),
        mode: mode.into(),
        content,
        disposition: "create_or_replace_if_different".into(),
        applicability,
    }
}

fn apply_remote() -> Applicability {
    Applicability::ApplyNow {
        phase: ExecutionPhase::RemoteInstall,
    }
}

fn deferred(roadmap_step: u8) -> Applicability {
    Applicability::Deferred { roadmap_step }
}

fn release_component(version: &str, executable_sha256: &str) -> String {
    format!("{version}-{executable_sha256}")
}

fn planned_service_transitions() -> Vec<ServiceTransition> {
    [
        ("sy-spark-executor.service", apply_remote()),
        ("sy-spark-agent.service", apply_remote()),
        ("sy-spark.target", apply_remote()),
    ]
    .into_iter()
    .map(|(unit, applicability)| ServiceTransition {
        unit: unit.into(),
        before: "inspected_current_state".into(),
        after: "enabled_and_active".into(),
        applicability,
    })
    .collect()
}

fn rejected_updates() -> Vec<RejectedUpdateClass> {
    use RejectedUpdateClass::*;
    vec![
        OperatingSystem,
        Kernel,
        NvidiaDriver,
        CudaRuntime,
        Firmware,
        Bootloader,
        Docker,
        NvidiaContainerToolkit,
        SystemPython,
        Sysctl,
        Swap,
        Clocks,
        Power,
        TransparentHugePages,
        Firewall,
        EngineImagePull,
        ModelDownload,
    ]
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallAction {
    EnsureIdentity(String),
    CreateDirectory(PathBuf),
    EnsureReleaseDirectory(PathBuf),
    VerifyReleaseArtifact(PathBuf),
    EnsurePolicyAsset(PathBuf),
    EnsureLocalIdentity(PathBuf),
    ActivateRelease(PathBuf),
    ApplyServiceTransition(String),
}

#[cfg(feature = "spark-agent")]
impl InstallAction {
    fn approved_asset_key(&self) -> (String, String) {
        let (kind, path) = match self {
            Self::EnsureIdentity(identity) => ("identity", identity.clone()),
            Self::CreateDirectory(path) | Self::EnsureReleaseDirectory(path) => {
                ("directory", absolute_asset_path(path))
            }
            Self::VerifyReleaseArtifact(path) => ("file", absolute_asset_path(path)),
            Self::EnsurePolicyAsset(path) => {
                let kind = if path.starts_with("etc/systemd/system") {
                    "systemdunit"
                } else if path.starts_with("etc/apparmor.d") {
                    "lsmpolicy"
                } else if path.starts_with("etc/sy/spark-recipes.d") {
                    "recipe"
                } else {
                    "file"
                };
                (kind, absolute_asset_path(path))
            }
            Self::EnsureLocalIdentity(path) => {
                let kind = if path
                    .extension()
                    .is_some_and(|extension| extension == "credential")
                {
                    "credential"
                } else {
                    "certificate"
                };
                (kind, absolute_asset_path(path))
            }
            Self::ActivateRelease(path) => ("symlink", absolute_asset_path(path)),
            Self::ApplyServiceTransition(unit) => ("service_transition", unit.clone()),
        };
        (kind.into(), path)
    }
}

#[cfg(feature = "spark-agent")]
fn absolute_asset_path(path: &Path) -> String {
    format!("/{}", path.to_string_lossy().trim_start_matches('/'))
}

#[cfg(feature = "spark-agent")]
pub fn validate_install_actions(
    manifest: &InstallManifest,
    actions: &[InstallAction],
) -> Result<(), InstallError> {
    let mut approved: std::collections::BTreeMap<_, usize> = manifest
        .assets
        .iter()
        .filter(|asset| asset.applicability == apply_remote())
        .map(|asset| {
            (
                format!("{:?}", asset.kind).to_ascii_lowercase(),
                asset.path_or_name.clone(),
            )
        })
        .map(|key| (key, 1))
        .collect();
    approved.extend(
        manifest
            .service_transitions
            .iter()
            .filter(|transition| transition.applicability == apply_remote())
            .map(|transition| (("service_transition".into(), transition.unit.clone()), 1)),
    );
    let mut executed = std::collections::BTreeMap::new();
    for key in actions.iter().map(InstallAction::approved_asset_key) {
        *executed.entry(key).or_insert(0) += 1;
    }
    if approved == executed {
        Ok(())
    } else {
        Err(configuration_error(format!(
            "executed install assets differ from approved manifest: approved={approved:?}, executed={executed:?}"
        )))
    }
}

#[cfg(feature = "spark-agent")]
pub struct ReleaseBundle<'a> {
    pub version: &'a str,
    pub executable: &'a [u8],
    pub executable_sha256: &'a str,
    pub public_key_base64: &'a str,
    pub signature: &'a str,
    pub listen_address: &'a str,
    pub hostname: &'a str,
    pub active_lsm: &'a str,
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub changed: bool,
    pub actions: Vec<InstallAction>,
    pub fsync_trace: Vec<PathBuf>,
    pub preceding_release: Option<PathBuf>,
    pub active_release: PathBuf,
}

pub struct BootstrapMaterial {
    pub ca_certificate_pem: String,
    pub ca_certificate_sha256: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceReport {
    pub schema: String,
    pub operation: String,
    pub dry_run: bool,
    pub applied: bool,
    pub current_release: String,
    pub target_release: String,
    pub state_schema: u32,
    pub database_integrity: String,
    pub verified_backup: Option<String>,
    pub active_recipes: Vec<String>,
    pub protected_stack_policy: String,
    pub healthy_engines_preserved: bool,
    pub docker_restart: String,
    pub host_reboot: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificateRotationReport {
    pub schema: String,
    pub dry_run: bool,
    pub applied: bool,
    pub rotated_ca: bool,
    pub overlap_preserved: bool,
    pub client_repin_required: bool,
    pub ca_certificate_sha256: String,
    pub leaf_certificate_sha256: String,
    pub ca_certificate_pem: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceCommand {
    Rollback,
    RotateLeaf,
    RotateCa,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivationMetadata {
    schema: String,
    changed: bool,
    preceding_release: Option<String>,
    active_release: String,
}

pub struct ActivationResult {
    pub changed: bool,
    pub preceding_release: Option<PathBuf>,
    pub active_release: PathBuf,
    pub material: BootstrapMaterial,
}

impl std::fmt::Debug for ActivationResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActivationResult")
            .field("changed", &self.changed)
            .field("preceding_release", &self.preceding_release)
            .field("active_release", &self.active_release)
            .field("material", &self.material)
            .finish()
    }
}

impl std::fmt::Debug for BootstrapMaterial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BootstrapMaterial")
            .field("ca_certificate_sha256", &self.ca_certificate_sha256)
            .field("ca_certificate_pem", &"[SSH-DELIVERED]")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

#[cfg(feature = "spark-agent")]
pub fn install_release(
    root: &Path,
    bundle: &ReleaseBundle<'_>,
) -> Result<(InstallReport, BootstrapMaterial), InstallError> {
    install_release_with_integration(root, bundle, || {
        if root == Path::new("/") {
            apply_host_integration()
        } else {
            Ok(())
        }
    })
}

#[cfg(feature = "spark-agent")]
pub fn rollback_release(
    root: &Path,
    dry_run: bool,
    integrate: impl FnOnce() -> Result<(), InstallError>,
) -> Result<MaintenanceReport, InstallError> {
    let current_path = root.join("opt/sy-spark/current");
    let previous_path = root.join("opt/sy-spark/previous");
    let current = checked_release_link(root, &current_path)?;
    let target = checked_release_link(root, &previous_path)?;
    if current == target {
        return Err(configuration_error("rollback target is already active"));
    }
    let (state_schema, database_integrity, verified_backup, active_recipes) =
        validate_rollback_state(root)?;
    let mut report = MaintenanceReport {
        schema: "sy.spark.maintenance/v1".into(),
        operation: "rollback".into(),
        dry_run,
        applied: false,
        current_release: current.to_string_lossy().into_owned(),
        target_release: target.to_string_lossy().into_owned(),
        state_schema,
        database_integrity,
        verified_backup,
        active_recipes,
        protected_stack_policy: "must_remain_byte_identical".into(),
        healthy_engines_preserved: true,
        docker_restart: "not_run".into(),
        host_reboot: "not_run".into(),
    };
    if dry_run {
        return Ok(report);
    }
    replace_symlink(&current_path, &target)?;
    replace_symlink(&previous_path, &current)?;
    if let Err(error) = integrate() {
        let _ = replace_symlink(&current_path, &current);
        let _ = replace_symlink(&previous_path, &target);
        return Err(error);
    }
    report.applied = true;
    Ok(report)
}

#[cfg(feature = "spark-agent")]
fn checked_release_link(root: &Path, path: &Path) -> Result<PathBuf, InstallError> {
    use std::path::Component;

    let target = std::fs::read_link(path).map_err(|error| {
        configuration_error(format!("read release link {}: {error}", path.display()))
    })?;
    let mut components = target.components();
    if components.next().and_then(|part| part.as_os_str().to_str()) != Some("releases")
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(configuration_error(
            "release link escapes the release directory",
        ));
    }
    let binary = root.join("opt/sy-spark").join(&target).join("sy");
    let bytes = std::fs::read(&binary).map_err(|error| {
        configuration_error(format!(
            "read rollback release {}: {error}",
            binary.display()
        ))
    })?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| configuration_error("release name is not UTF-8"))?;
    let expected = name
        .rsplit_once('-')
        .map(|(_, digest)| digest)
        .filter(|digest| is_sha256_text(digest))
        .ok_or_else(|| configuration_error("release name has no exact SHA-256"))?;
    if format!("{:x}", Sha256::digest(bytes)) != expected {
        return Err(configuration_error(
            "rollback release artifact hash mismatch",
        ));
    }
    Ok(target)
}

#[cfg(feature = "spark-agent")]
fn validate_rollback_state(
    root: &Path,
) -> Result<(u32, String, Option<String>, Vec<String>), InstallError> {
    use rusqlite::{Connection, OpenFlags};
    let database = root.join("var/lib/sy-spark/state.sqlite3");
    if !database.exists() {
        return Ok((0, "absent".into(), None, Vec::new()));
    }
    let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| configuration_error(format!("open state database read-only: {error}")))?;
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|error| configuration_error(format!("verify state database: {error}")))?;
    if integrity != "ok" {
        return Err(configuration_error("state database integrity check failed"));
    }
    let schema: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| configuration_error(format!("read state schema: {error}")))?;
    if !(4..=5).contains(&schema) {
        return Err(configuration_error(format!(
            "state schema {schema} is outside the N/N-1 rollback window"
        )));
    }
    let catalog =
        crate::spark::recipe::RecipeCatalog::load_signed(&root.join("etc/sy/spark-recipes.d"))
            .map_err(configuration_error)?;
    let mut statement = connection
        .prepare("SELECT metadata_json FROM instances WHERE desired_state='running' ORDER BY name")
        .map_err(|error| configuration_error(format!("read active instances: {error}")))?;
    let mut active_recipes = Vec::new();
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| configuration_error(format!("read active instance rows: {error}")))?;
    for row in rows {
        let metadata =
            row.map_err(|error| configuration_error(format!("read active instance: {error}")))?;
        let instance: super::wire::InstanceDocument = serde_json::from_str(&metadata)
            .map_err(|error| configuration_error(format!("decode active instance: {error}")))?;
        if catalog.recipe(&instance.recipe_id).is_none() {
            return Err(configuration_error(format!(
                "active instance {} requires unavailable recipe {}",
                instance.name, instance.recipe_id
            )));
        }
        active_recipes.push(instance.recipe_id);
    }
    active_recipes.sort();
    active_recipes.dedup();
    let verified_backup = newest_verified_backup(root)?;
    Ok((schema, integrity, verified_backup, active_recipes))
}

#[cfg(feature = "spark-agent")]
fn newest_verified_backup(root: &Path) -> Result<Option<String>, InstallError> {
    use rusqlite::{Connection, OpenFlags};
    let directory = root.join("var/lib/sy-spark/backups");
    let mut paths = match std::fs::read_dir(&directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|value| value == "sqlite3"))
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(configuration_error(format!(
                "read database backups: {error}"
            )))
        }
    };
    paths.sort();
    for path in paths.into_iter().rev() {
        let Ok(connection) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            continue;
        };
        let integrity =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0));
        if integrity.as_deref() == Ok("ok") {
            return Ok(path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned));
        }
    }
    Err(configuration_error(
        "installed state database has no verified rollback backup",
    ))
}

#[cfg(feature = "spark-agent")]
pub fn rotate_certificate(
    root: &Path,
    dry_run: bool,
    rotate_ca: bool,
    integrate: impl FnOnce() -> Result<(), InstallError>,
) -> Result<CertificateRotationReport, InstallError> {
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
        IsCa, Issuer, KeyPair, KeyUsagePurpose,
    };
    let ca_cert_path = root.join("var/lib/sy-spark/ca/ca-cert.pem");
    let ca_key_path = root.join("var/lib/sy-spark/ca/ca-key.pem");
    let leaf_cert_path = root.join("var/lib/sy-spark/tls/server-chain.pem");
    let leaf_key_path = root.join("var/lib/sy-spark/tls/server-key.pem");
    for path in [&ca_cert_path, &ca_key_path, &leaf_cert_path, &leaf_key_path] {
        if !path.is_file() {
            return Err(configuration_error(format!(
                "certificate rotation prerequisite is missing: {}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("asset")
            )));
        }
    }
    let (address, hostname) = installed_listener_identity(root)?;
    let old_ca = std::fs::read_to_string(&ca_cert_path)
        .map_err(|error| configuration_error(format!("read local CA certificate: {error}")))?;
    let old_ca_key = std::fs::read_to_string(&ca_key_path)
        .map_err(|error| configuration_error(format!("read local CA key: {error}")))?;
    let old_leaf = std::fs::read(&leaf_cert_path)
        .map_err(|error| configuration_error(format!("read current leaf: {error}")))?;
    let old_leaf_key = std::fs::read(&leaf_key_path)
        .map_err(|error| configuration_error(format!("read current leaf key: {error}")))?;
    let ca_key = if rotate_ca {
        KeyPair::generate()
            .map_err(|error| configuration_error(format!("generate replacement CA key: {error}")))?
    } else {
        KeyPair::from_pem(&old_ca_key)
            .map_err(|error| configuration_error(format!("parse local CA key: {error}")))?
    };
    let mut ca_params = CertificateParams::new(Vec::<String>::new())
        .map_err(|error| configuration_error(format!("build local CA: {error}")))?;
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "sy Spark local CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_pem = if rotate_ca {
        ca_params
            .self_signed(&ca_key)
            .map_err(|error| configuration_error(format!("sign replacement CA: {error}")))?
            .pem()
    } else {
        old_ca.clone()
    };
    let leaf_key = KeyPair::generate()
        .map_err(|error| configuration_error(format!("generate replacement leaf key: {error}")))?;
    let mut leaf_params = CertificateParams::new(vec![address, hostname.clone()])
        .map_err(|error| configuration_error(format!("build replacement leaf: {error}")))?;
    leaf_params.distinguished_name = DistinguishedName::new();
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, hostname);
    leaf_params.is_ca = IsCa::ExplicitNoCa;
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let leaf_pem = leaf_params
        .signed_by(&leaf_key, &issuer)
        .map_err(|error| configuration_error(format!("sign replacement leaf: {error}")))?
        .pem();
    let chain = format!("{leaf_pem}{ca_pem}");
    let mut report = CertificateRotationReport {
        schema: "sy.spark.certificate-rotation/v1".into(),
        dry_run,
        applied: false,
        rotated_ca: rotate_ca,
        overlap_preserved: true,
        client_repin_required: rotate_ca,
        ca_certificate_sha256: format!("sha256:{:x}", Sha256::digest(ca_pem.as_bytes())),
        leaf_certificate_sha256: format!("sha256:{:x}", Sha256::digest(leaf_pem.as_bytes())),
        ca_certificate_pem: rotate_ca.then_some(ca_pem.clone()),
    };
    if dry_run {
        return Ok(report);
    }
    let mut trace = Vec::new();
    for (path, bytes, mode) in [
        (
            ca_cert_path.with_extension("pem.overlap"),
            old_ca.as_bytes(),
            0o644,
        ),
        (
            ca_key_path.with_extension("pem.overlap"),
            old_ca_key.as_bytes(),
            0o600,
        ),
        (
            leaf_cert_path.with_extension("pem.overlap"),
            old_leaf.as_slice(),
            0o644,
        ),
        (
            leaf_key_path.with_extension("pem.overlap"),
            old_leaf_key.as_slice(),
            0o600,
        ),
    ] {
        write_synced(&path, bytes, mode, &mut trace)?;
    }
    if rotate_ca {
        write_synced(&ca_cert_path, ca_pem.as_bytes(), 0o644, &mut trace)?;
        write_synced(
            &ca_key_path,
            ca_key.serialize_pem().as_bytes(),
            0o600,
            &mut trace,
        )?;
    }
    write_synced(&leaf_cert_path, chain.as_bytes(), 0o644, &mut trace)?;
    write_synced(
        &leaf_key_path,
        leaf_key.serialize_pem().as_bytes(),
        0o600,
        &mut trace,
    )?;
    if let Err(error) = integrate() {
        let mut restore_trace = Vec::new();
        let _ = write_synced(&ca_cert_path, old_ca.as_bytes(), 0o644, &mut restore_trace);
        let _ = write_synced(
            &ca_key_path,
            old_ca_key.as_bytes(),
            0o600,
            &mut restore_trace,
        );
        let _ = write_synced(&leaf_cert_path, &old_leaf, 0o644, &mut restore_trace);
        let _ = write_synced(&leaf_key_path, &old_leaf_key, 0o600, &mut restore_trace);
        return Err(error);
    }
    report.applied = true;
    Ok(report)
}

#[cfg(feature = "spark-agent")]
fn installed_listener_identity(root: &Path) -> Result<(String, String), InstallError> {
    let config = std::fs::read_to_string(root.join("etc/sy/spark-agent.toml"))
        .map_err(|error| configuration_error(format!("read installed agent policy: {error}")))?;
    let listen = config
        .lines()
        .find_map(|line| line.trim().strip_prefix("listen = \"")?.strip_suffix('"'))
        .ok_or_else(|| configuration_error("installed agent policy has no fixed listener"))?;
    let address = listen
        .rsplit_once(':')
        .map(|(address, _)| address)
        .filter(|address| !address.is_empty())
        .ok_or_else(|| configuration_error("installed agent listener is invalid"))?;
    let hostname = std::fs::read_to_string(root.join("etc/hostname"))
        .map_err(|error| configuration_error(format!("read installed hostname: {error}")))?
        .trim()
        .to_owned();
    if hostname.is_empty() {
        return Err(configuration_error("installed hostname is empty"));
    }
    Ok((address.into(), hostname))
}

#[cfg(feature = "spark-agent")]
fn install_release_with_integration(
    root: &Path,
    bundle: &ReleaseBundle<'_>,
    integrate: impl FnOnce() -> Result<(), InstallError>,
) -> Result<(InstallReport, BootstrapMaterial), InstallError> {
    use std::os::unix::fs::{symlink, PermissionsExt};

    validate_release(bundle)?;
    if bundle.active_lsm != "apparmor:enforce" {
        return Err(configuration_error(format!(
            "active LSM {} cannot be enforced by this release",
            bundle.active_lsm
        )));
    }
    let release_name = release_component(bundle.version, bundle.executable_sha256);
    let release_rel = PathBuf::from("opt/sy-spark/releases").join(&release_name);
    let release = root.join(&release_rel);
    let current = root.join("opt/sy-spark/current");
    let previous = root.join("opt/sy-spark/previous");
    let preceding_release = std::fs::read_link(&current).ok();
    if release.exists() {
        let installed = std::fs::read(release.join("sy"))
            .map_err(|error| configuration_error(format!("read installed release: {error}")))?;
        if format!("{:x}", Sha256::digest(installed)) != bundle.executable_sha256 {
            return Err(configuration_error(
                "installed release hash differs from signed release",
            ));
        }
        if preceding_release.as_ref() == Some(&PathBuf::from("releases").join(&release_name)) {
            let material = existing_bootstrap_material(root)?;
            return Ok((
                InstallReport {
                    changed: false,
                    actions: Vec::new(),
                    fsync_trace: Vec::new(),
                    preceding_release,
                    active_release: release_rel,
                },
                material,
            ));
        }
    }
    let rollback = TransactionSnapshot::capture(root, &release_rel)?;

    let mut report = InstallReport {
        changed: true,
        actions: vec![
            InstallAction::EnsureIdentity("user:sy-spark".into()),
            InstallAction::EnsureIdentity("group:sy-spark".into()),
        ],
        fsync_trace: Vec::new(),
        preceding_release: preceding_release.clone(),
        active_release: release_rel.clone(),
    };
    if root == Path::new("/") {
        ensure_service_identity()?;
    }
    for (relative, mode, approved) in directory_layout() {
        let path = root.join(relative);
        std::fs::create_dir_all(&path)
            .map_err(|error| configuration_error(format!("create {}: {error}", path.display())))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| configuration_error(format!("chmod {}: {error}", path.display())))?;
        if approved {
            report
                .actions
                .push(InstallAction::CreateDirectory(relative.into()));
        }
    }

    if !release.exists() {
        let stage = root.join("opt/sy-spark/releases").join(format!(
            ".stage-{}-{}",
            release_name,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&stage)
            .map_err(|error| configuration_error(format!("create release stage: {error}")))?;
        write_synced(
            &stage.join("sy"),
            bundle.executable,
            0o555,
            &mut report.fsync_trace,
        )?;
        sync_dir(&stage, &mut report.fsync_trace)?;
        std::fs::rename(&stage, &release).map_err(|error| {
            configuration_error(format!("activate staged release directory: {error}"))
        })?;
        sync_dir(&root.join("opt/sy-spark/releases"), &mut report.fsync_trace)?;
    }
    report
        .actions
        .push(InstallAction::EnsureReleaseDirectory(release_rel.clone()));
    report
        .actions
        .push(InstallAction::VerifyReleaseArtifact(release_rel.join("sy")));

    let service_uid = installed_service_uid(root)?;
    write_policy_assets(root, bundle.listen_address, service_uid, &mut report)?;
    let material =
        ensure_local_identity(root, bundle.listen_address, bundle.hostname, &mut report)?;
    if let Some(preceding) = preceding_release.as_ref() {
        replace_symlink(&previous, preceding)?;
        report
            .actions
            .push(InstallAction::ActivateRelease(PathBuf::from(
                "opt/sy-spark/previous",
            )));
    }
    let link = root
        .join("opt/sy-spark")
        .join(format!(".current-{}", uuid::Uuid::new_v4()));
    symlink(PathBuf::from("releases").join(&release_name), &link)
        .map_err(|error| configuration_error(format!("stage current symlink: {error}")))?;
    std::fs::rename(&link, &current).map_err(|error| {
        configuration_error(format!("atomically activate current symlink: {error}"))
    })?;
    sync_dir(&root.join("opt/sy-spark"), &mut report.fsync_trace)?;
    report
        .actions
        .push(InstallAction::ActivateRelease(PathBuf::from(
            "opt/sy-spark/current",
        )));
    if let Err(error) = integrate() {
        rollback.restore(root)?;
        return Err(error);
    }
    report.actions.push(InstallAction::ApplyServiceTransition(
        "sy-spark-executor.service".into(),
    ));
    report.actions.push(InstallAction::ApplyServiceTransition(
        "sy-spark-agent.service".into(),
    ));
    report.actions.push(InstallAction::ApplyServiceTransition(
        "sy-spark.target".into(),
    ));
    Ok((report, material))
}

#[cfg(feature = "spark-agent")]
type FileSnapshot = (PathBuf, Option<(Vec<u8>, u32)>);

#[cfg(feature = "spark-agent")]
struct TransactionSnapshot {
    files: Vec<FileSnapshot>,
    current: Option<PathBuf>,
    previous: Option<PathBuf>,
    release_existed: bool,
    release_relative: PathBuf,
    created_directories: Vec<PathBuf>,
}

#[cfg(feature = "spark-agent")]
impl TransactionSnapshot {
    fn capture(root: &Path, release_relative: &Path) -> Result<Self, InstallError> {
        use std::os::unix::fs::PermissionsExt;
        let file_paths = [
            "etc/sy/spark-agent.toml",
            "etc/sy/spark-executor.toml",
            "etc/systemd/system/sy-spark-agent.service",
            "etc/systemd/system/sy-spark-executor.service",
            "etc/systemd/system/sy-spark.target",
            "etc/apparmor.d/sy-spark-agent",
            "etc/apparmor.d/sy-spark-executor",
            "etc/sy/spark-recipes.d/ornith-vllm.toml",
            "etc/sy/spark-recipes.d/qwen3-embedding.toml",
            "etc/sy/spark-recipes.d/fixture-http-echo.toml",
            "var/lib/sy-spark/ca/ca-key.pem",
            "var/lib/sy-spark/ca/ca-cert.pem",
            "var/lib/sy-spark/tls/server-key.pem",
            "var/lib/sy-spark/tls/server-chain.pem",
            "etc/sy/spark-bootstrap-admin.credential",
            "etc/sy/spark-hf-read.credential",
        ];
        let mut files = Vec::new();
        for relative in file_paths {
            let path = root.join(relative);
            let prior = match std::fs::read(&path) {
                Ok(bytes) => Some((
                    bytes,
                    std::fs::metadata(&path)
                        .map_err(|error| {
                            configuration_error(format!("stat rollback path: {error}"))
                        })?
                        .permissions()
                        .mode()
                        & 0o777,
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(configuration_error(format!(
                        "snapshot rollback path {}: {error}",
                        path.display()
                    )))
                }
            };
            files.push((PathBuf::from(relative), prior));
        }
        let current_path = root.join("opt/sy-spark/current");
        let current = std::fs::read_link(&current_path).ok();
        let previous = std::fs::read_link(root.join("opt/sy-spark/previous")).ok();
        let release_relative = release_relative.to_path_buf();
        let release_existed = root.join(&release_relative).exists();
        let created_directories = directory_layout()
            .into_iter()
            .filter(|(relative, _, _)| !root.join(relative).exists())
            .map(|(relative, _, _)| relative.into())
            .collect();
        Ok(Self {
            files,
            current,
            previous,
            release_existed,
            release_relative,
            created_directories,
        })
    }

    fn restore(self, root: &Path) -> Result<(), InstallError> {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let current = root.join("opt/sy-spark/current");
        if std::fs::symlink_metadata(&current).is_ok() {
            std::fs::remove_file(&current).map_err(|error| {
                configuration_error(format!("remove failed activation: {error}"))
            })?;
        }
        if let Some(target) = self.current {
            symlink(target, &current).map_err(|error| {
                configuration_error(format!("restore preceding activation: {error}"))
            })?;
        }
        let previous = root.join("opt/sy-spark/previous");
        if std::fs::symlink_metadata(&previous).is_ok() {
            std::fs::remove_file(&previous).map_err(|error| {
                configuration_error(format!("remove failed previous activation: {error}"))
            })?;
        }
        if let Some(target) = self.previous {
            symlink(target, &previous).map_err(|error| {
                configuration_error(format!("restore previous activation: {error}"))
            })?;
        }
        for (relative, prior) in self.files {
            let path = root.join(relative);
            match prior {
                Some((bytes, mode)) => {
                    std::fs::write(&path, bytes).map_err(|error| {
                        configuration_error(format!("restore {}: {error}", path.display()))
                    })?;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                        .map_err(|error| {
                            configuration_error(format!(
                                "chmod restored {}: {error}",
                                path.display()
                            ))
                        })?;
                    File::open(&path)
                        .and_then(|file| file.sync_all())
                        .map_err(|error| {
                            configuration_error(format!("fsync restored file: {error}"))
                        })?;
                }
                None if std::fs::symlink_metadata(&path).is_ok() => {
                    std::fs::remove_file(&path).map_err(|error| {
                        configuration_error(format!("remove new {}: {error}", path.display()))
                    })?;
                }
                None => {}
            }
        }
        if !self.release_existed && root.join(&self.release_relative).exists() {
            std::fs::remove_dir_all(root.join(&self.release_relative))
                .map_err(|error| configuration_error(format!("remove failed release: {error}")))?;
        }
        if let Some(parent) = current.parent() {
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| configuration_error(format!("fsync rollback root: {error}")))?;
        }
        for relative in self.created_directories.into_iter().rev() {
            let path = root.join(relative);
            if path.exists() {
                std::fs::remove_dir(&path).map_err(|error| {
                    configuration_error(format!("remove new directory {}: {error}", path.display()))
                })?;
            }
        }
        Ok(())
    }
}

#[cfg(feature = "spark-agent")]
fn ensure_service_identity() -> Result<(), InstallError> {
    if !fixed_success("getent", &["group", "sy-spark"])? {
        require_fixed_success(
            "create sy-spark service group",
            "groupadd",
            &["--system", "sy-spark"],
        )?;
    }
    if !fixed_success("getent", &["passwd", "sy-spark"])? {
        require_fixed_success(
            "create sy-spark service user",
            "useradd",
            &[
                "--system",
                "--gid",
                "sy-spark",
                "--home-dir",
                "/nonexistent",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                "sy-spark",
            ],
        )?;
    }
    Ok(())
}

#[cfg(feature = "spark-agent")]
fn installed_service_uid(root: &Path) -> Result<u32, InstallError> {
    if root != Path::new("/") {
        return Ok(996);
    }
    let passwd = std::fs::read_to_string("/etc/passwd")
        .map_err(|error| configuration_error(format!("read service identity database: {error}")))?;
    passwd
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            (fields.next() == Some("sy-spark"))
                .then(|| fields.nth(1)?.parse::<u32>().ok())
                .flatten()
        })
        .ok_or_else(|| configuration_error("installed sy-spark identity has no numeric UID"))
}

#[cfg(feature = "spark-agent")]
fn apply_host_integration() -> Result<(), InstallError> {
    ensure_http_fallback()?;
    require_fixed_success(
        "set Spark state ownership",
        "chown",
        &["sy-spark:sy-spark", "/var/lib/sy-spark"],
    )?;
    require_fixed_success(
        "set agent cache and TLS ownership",
        "chown",
        &[
            "-R",
            "sy-spark:sy-spark",
            "/var/lib/sy-spark/huggingface",
            "/var/lib/sy-spark/tls",
        ],
    )?;
    require_fixed_success(
        "set executor boundary ownership",
        "chown",
        &[
            "root:sy-spark",
            "/etc/sy",
            "/etc/sy/spark-agent.toml",
            "/etc/sy/spark-executor.toml",
            "/etc/sy/spark-hf-read.credential",
            "/run/sy-spark",
            "/var/lib/sy-spark/executor",
            "/var/lib/sy-spark/compile-cache",
        ],
    )?;
    require_fixed_success(
        "reload agent AppArmor profile",
        "apparmor_parser",
        &["-r", "/etc/apparmor.d/sy-spark-agent"],
    )?;
    require_fixed_success(
        "reload executor AppArmor profile",
        "apparmor_parser",
        &["-r", "/etc/apparmor.d/sy-spark-executor"],
    )?;
    require_fixed_success(
        "reload systemd unit definitions",
        "systemctl",
        &["daemon-reload"],
    )?;
    require_fixed_success(
        "enable and start Spark supervision target",
        "systemctl",
        target_enable_args(),
    )?;
    restart_control_plane()
}

#[cfg(feature = "spark-agent")]
fn target_enable_args() -> &'static [&'static str] {
    &["enable", "--now", "sy-spark.target"]
}

#[cfg(feature = "spark-agent")]
fn restart_control_plane() -> Result<(), InstallError> {
    require_fixed_success(
        "start or restart Spark executor",
        "systemctl",
        &["restart", "sy-spark-executor.service"],
    )?;
    require_fixed_success(
        "restart Spark agent on the activated release",
        "systemctl",
        &["restart", "sy-spark-agent.service"],
    )
}

#[cfg(feature = "spark-agent")]
fn ensure_http_fallback() -> Result<(), InstallError> {
    use std::os::unix::fs::symlink;

    let lock = include_bytes!("../../configs/sy/spark/hf-http-fallback.lock");
    let digest = format!("{:x}", Sha256::digest(lock));
    let root = PathBuf::from("/opt/sy-spark/hf-http-fallback");
    let release = root.join(&digest);
    let executable = release.join("venv/bin/huggingface-cli");
    if !executable.is_file() {
        let release_text = release.to_string_lossy().into_owned();
        require_fixed_success(
            "create isolated Hugging Face fallback venv",
            "python3",
            &["-m", "venv", &format!("{release_text}/venv")],
        )?;
        let python = format!("{release_text}/venv/bin/python");
        let requirements = format!("{release_text}/requirements.lock");
        require_fixed_success(
            "install hash-locked official Hugging Face fallback",
            &python,
            &[
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--require-hashes",
                "-r",
                &requirements,
            ],
        )?;
        if !executable.is_file() {
            return Err(configuration_error(
                "hash-locked Hugging Face fallback did not install its fixed executable",
            ));
        }
    }
    let current = root.join("current");
    if std::fs::read_link(&current).ok().as_deref() != Some(Path::new(&digest)) {
        let staged = root.join(format!(".current-{}", uuid::Uuid::new_v4()));
        symlink(&digest, &staged).map_err(|error| {
            configuration_error(format!("stage HTTP fallback activation: {error}"))
        })?;
        std::fs::rename(&staged, &current)
            .map_err(|error| configuration_error(format!("activate HTTP fallback: {error}")))?;
    }
    Ok(())
}

#[cfg(feature = "spark-agent")]
fn fixed_success(program: &str, args: &[&str]) -> Result<bool, InstallError> {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .map_err(|error| {
            configuration_error(format!("run fixed installer action {program}: {error}"))
        })
}

#[cfg(feature = "spark-agent")]
fn require_fixed_success(action: &str, program: &str, args: &[&str]) -> Result<(), InstallError> {
    let output = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            configuration_error(format!(
                "fixed host-integration action {action} could not start (program={program}): {error}"
            ))
        })?;
    if output.status.success() {
        Ok(())
    } else {
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".into());
        let diagnostic = sanitize_diagnostic(&output.stderr);
        Err(configuration_error(format!(
            "fixed host-integration action {action} failed (program={program}, status={status}, stderr={diagnostic})"
        )))
    }
}

const DIAGNOSTIC_LIMIT: usize = 512;

fn sanitize_diagnostic(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut redacted = Vec::new();
    let mut redact_next = false;
    for token in text.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        let sensitive = redact_next
            || lower.contains("token=")
            || lower.contains("secret=")
            || lower.contains("credential=")
            || lower.contains("authorization:")
            || lower.starts_with("spark-bootstrap.")
            || token.contains('{')
            || token.contains('}')
            || token.len() > 96;
        redacted.push(if sensitive { "[REDACTED]" } else { token });
        redact_next = lower == "bearer" || lower.ends_with("bearer:");
    }
    let single_line = redacted.join(" ");
    if single_line.chars().count() <= DIAGNOSTIC_LIMIT {
        return if single_line.is_empty() {
            "none".into()
        } else {
            single_line
        };
    }
    format!(
        "{} [truncated]",
        single_line
            .chars()
            .take(DIAGNOSTIC_LIMIT)
            .collect::<String>()
    )
}

#[cfg(feature = "spark-agent")]
pub fn activate_from_files(
    request: &ActivateRequest,
) -> Result<(InstallReport, BootstrapMaterial), InstallError> {
    let executable = std::fs::read(&request.executable)
        .map_err(|error| configuration_error(format!("read staged release: {error}")))?;
    let signature = std::fs::read_to_string(&request.signature)
        .map_err(|error| configuration_error(format!("read staged release signature: {error}")))?;
    let public_key = std::fs::read_to_string(&request.public_key)
        .map_err(|error| configuration_error(format!("read pinned release public key: {error}")))?;
    let manifest_bytes = std::fs::read(&request.manifest)
        .map_err(|error| configuration_error(format!("read approved install manifest: {error}")))?;
    if format!("{:x}", Sha256::digest(&manifest_bytes)) != request.manifest_sha256 {
        return Err(configuration_error(
            "approved install manifest SHA-256 mismatch",
        ));
    }
    let manifest: InstallManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        configuration_error(format!("decode approved install manifest: {error}"))
    })?;
    if manifest.execution != super::wire::InstallExecution::Planned
        || manifest.approval_sha256
            != manifest_approval_sha256(&manifest).map_err(configuration_error)?
    {
        return Err(configuration_error(
            "approved install manifest is not its canonical planned projection",
        ));
    }
    let executable_sha256 = format!("{:x}", Sha256::digest(&executable));
    let bundle = ReleaseBundle {
        version: &request.version,
        executable: &executable,
        executable_sha256: &executable_sha256,
        public_key_base64: public_key.trim(),
        signature: &signature,
        listen_address: &request.listen_address,
        hostname: &request.hostname,
        active_lsm: &request.active_lsm,
    };
    if manifest.probe.local_sha256 != executable_sha256
        || manifest.listen_address != request.listen_address
        || manifest.inventory.hostname != request.hostname
        || format!(
            "{}:{}",
            manifest.inventory.lsm.kind, manifest.inventory.lsm.mode
        ) != request.active_lsm
    {
        return Err(configuration_error(
            "activation request differs from the approved manifest",
        ));
    }
    if request.root == Path::new("/") && manifest.inventory.existing_installation.present {
        validate_rollback_state(&request.root)?;
    }
    let installed = install_release(&request.root, &bundle)?;
    if installed.0.changed {
        validate_install_actions(&manifest, &installed.0.actions)?;
    }
    if request.root == Path::new("/") {
        let after = ProtectedFingerprint::from_inventory(&bootstrap_inventory()?)
            .map_err(configuration_error)?;
        if after != manifest.protected_before {
            let _ = rollback_installed(false);
            return Err(configuration_error(
                "protected DGX stack changed during activation; control plane was rolled back",
            ));
        }
    }
    Ok(installed)
}

#[cfg(feature = "spark-agent")]
pub fn write_bootstrap_channel(
    report: &InstallReport,
    material: &BootstrapMaterial,
    mut output: impl Write,
) -> Result<(), InstallError> {
    let metadata = serde_json::to_vec(&ActivationMetadata {
        schema: "sy.spark.activation-result/v1".into(),
        changed: report.changed,
        preceding_release: report
            .preceding_release
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        active_release: report.active_release.to_string_lossy().into_owned(),
    })
    .map_err(|error| configuration_error(format!("encode activation result: {error}")))?;
    for field in [
        metadata.as_slice(),
        material.ca_certificate_pem.as_bytes(),
        material.ca_certificate_sha256.as_bytes(),
        material.token.as_bytes(),
    ] {
        writeln!(output, "{}", encode_hex(field)).map_err(|error| {
            configuration_error(format!("write protected SSH bootstrap channel: {error}"))
        })?;
    }
    Ok(())
}

#[cfg(feature = "spark-agent")]
fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0xf) as usize] as char);
    }
    output
}

fn decode_hex(value: &str) -> Result<Vec<u8>, InstallError> {
    if !value.len().is_multiple_of(2) {
        return Err(configuration_error(
            "invalid protected SSH bootstrap channel",
        ));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char)
                .to_digit(16)
                .ok_or_else(|| configuration_error("invalid protected SSH bootstrap channel"))?;
            let low = (pair[1] as char)
                .to_digit(16)
                .ok_or_else(|| configuration_error("invalid protected SSH bootstrap channel"))?;
            Ok(((high << 4) | low) as u8)
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct RemoteActivation<'a> {
    pub host_alias: &'a str,
    pub executable: &'a Path,
    pub signature: &'a Path,
    pub public_key: &'a Path,
    pub version: &'a str,
    pub executable_sha256: &'a str,
    pub listen_address: &'a str,
    pub hostname: &'a str,
    pub active_lsm: &'a str,
    pub manifest: &'a InstallManifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchStream {
    Inherit,
    Capture,
}

const SUDO_PROMPT: &str = "SY_SPARK_SUDO_PROMPT:";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActivationLaunchSpec {
    program: &'static str,
    args: Vec<String>,
    stdin: LaunchStream,
    stdout: LaunchStream,
    stderr: LaunchStream,
}

struct ActivationLaunchInput<'a> {
    host: &'a str,
    prefix: &'a str,
    signature: &'a str,
    public_key: &'a str,
    manifest: &'a str,
    manifest_sha256: &'a str,
    version: &'a str,
    listen_address: &'a str,
    hostname: &'a str,
    active_lsm: &'a str,
}

fn activation_launch_spec(input: ActivationLaunchInput<'_>) -> ActivationLaunchSpec {
    ActivationLaunchSpec {
        program: "ssh",
        args: [
            "-tt",
            "--",
            input.host,
            "sudo",
            "-p",
            SUDO_PROMPT,
            "--",
            input.prefix,
            "spark",
            "bootstrap",
            "activate",
            "--executable",
            input.prefix,
            "--signature",
            input.signature,
            "--public-key",
            input.public_key,
            "--manifest",
            input.manifest,
            "--manifest-sha256",
            input.manifest_sha256,
            "--version",
            input.version,
            "--listen-address",
            input.listen_address,
            "--hostname",
            input.hostname,
            "--active-lsm",
            input.active_lsm,
        ]
        .map(str::to_owned)
        .into(),
        stdin: LaunchStream::Inherit,
        stdout: LaunchStream::Capture,
        stderr: LaunchStream::Capture,
    }
}

fn run_activation_process(spec: &ActivationLaunchSpec) -> std::io::Result<std::process::Output> {
    let mut command = Command::new(spec.program);
    command.args(&spec.args);
    command.stdin(match spec.stdin {
        LaunchStream::Inherit => Stdio::inherit(),
        LaunchStream::Capture => Stdio::piped(),
    });
    command.stdout(match spec.stdout {
        LaunchStream::Inherit => Stdio::inherit(),
        LaunchStream::Capture => Stdio::piped(),
    });
    command.stderr(match spec.stderr {
        LaunchStream::Inherit => Stdio::inherit(),
        LaunchStream::Capture => Stdio::piped(),
    });
    let mut child = command.spawn()?;
    let stderr_task = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut captured = Vec::new();
            let mut visible = std::io::stderr();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stderr.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                captured.extend_from_slice(&chunk[..read]);
                visible.write_all(&chunk[..read])?;
                visible.flush()?;
            }
            Ok::<_, std::io::Error>(captured)
        })
    });
    let stdout = match child.stdout.take() {
        Some(stdout) => capture_activation_stdout(stdout, std::io::stderr())?,
        None => Vec::new(),
    };
    let status = child.wait()?;
    let stderr = stderr_task
        .map(|task| {
            task.join()
                .map_err(|_| std::io::Error::other("activation stderr reader panicked"))?
        })
        .transpose()?
        .unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn capture_activation_stdout(
    mut reader: impl Read,
    mut prompt_sink: impl Write,
) -> std::io::Result<Vec<u8>> {
    let prompt = SUDO_PROMPT.as_bytes();
    let mut captured = Vec::new();
    while captured.len() < prompt.len() {
        let mut byte = [0_u8; 1];
        if reader.read(&mut byte)? == 0 {
            return Ok(captured);
        }
        captured.push(byte[0]);
        if !prompt.starts_with(&captured) {
            reader.read_to_end(&mut captured)?;
            return Ok(captured);
        }
    }
    prompt_sink.write_all(prompt)?;
    prompt_sink.flush()?;
    reader.read_to_end(&mut captured)?;
    Ok(captured)
}

pub fn activate_over_ssh(request: &RemoteActivation<'_>) -> Result<ActivationResult, InstallError> {
    let prefix = activation_remote_prefix(request.executable_sha256)?;
    let signature_remote = format!("{prefix}.minisig");
    let public_key_remote = format!("{prefix}.pub");
    let manifest_remote = format!("{prefix}.manifest.json");
    let mut approved_manifest = request.manifest.clone();
    approved_manifest.execution = super::wire::InstallExecution::Planned;
    let manifest_bytes = serde_json::to_vec(&approved_manifest).map_err(|error| {
        configuration_error(format!("encode approved install manifest: {error}"))
    })?;
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
    let local_manifest = std::env::temp_dir().join(format!(
        "sy-spark-manifest-{manifest_sha256}-{}.json",
        uuid::Uuid::new_v4()
    ));
    let local_manifest = write_local_manifest(local_manifest, &manifest_bytes)?;
    let batch = format!(
        "put \"{}\" {prefix}\nput \"{}\" {signature_remote}\nput \"{}\" {public_key_remote}\nput \"{}\" {manifest_remote}\nchmod 0700 {prefix}\n",
        quote_sftp_path(request.executable)?, quote_sftp_path(request.signature)?, quote_sftp_path(request.public_key)?, quote_sftp_path(local_manifest.path())?,
    );
    let cleanup = format!(
        "-rm {prefix}\n-rm {signature_remote}\n-rm {public_key_remote}\n-rm {manifest_remote}\n"
    );
    if let Err(error) = run_release_sftp(request.host_alias, &batch) {
        let _ = run_release_sftp(request.host_alias, &cleanup);
        return Err(error);
    }
    let launch = activation_launch_spec(ActivationLaunchInput {
        host: request.host_alias,
        prefix: &prefix,
        signature: &signature_remote,
        public_key: &public_key_remote,
        manifest: &manifest_remote,
        manifest_sha256: &manifest_sha256,
        version: request.version,
        listen_address: request.listen_address,
        hostname: request.hostname,
        active_lsm: request.active_lsm,
    });
    let output = run_activation_process(&launch).map_err(|error| InstallError {
        kind: InstallErrorKind::Unreachable,
        message: format!("start fixed Spark activation over SSH: {error}"),
    });
    let cleanup_result = run_release_sftp(request.host_alias, &cleanup);
    let output = output?;
    if !output.status.success() {
        return Err(activation_exit_error(
            output.status,
            &output.stderr,
            &output.stdout,
        ));
    }
    cleanup_result?;
    parse_bootstrap_channel(&output.stdout)
}

fn activation_remote_prefix(executable_sha256: &str) -> Result<String, InstallError> {
    if !is_sha256_text(executable_sha256) {
        return Err(configuration_error("release SHA-256 is invalid"));
    }
    Ok(format!("/tmp/sy-spark-bootstrap-{executable_sha256}"))
}

pub fn maintenance_over_ssh(
    host: &str,
    command: MaintenanceCommand,
    dry_run: bool,
) -> Result<String, InstallError> {
    let mut args = vec![
        "-tt".into(),
        "--".into(),
        host.into(),
        "sudo".into(),
        "-p".into(),
        SUDO_PROMPT.into(),
        "--".into(),
        "/opt/sy-spark/current/sy".into(),
        "spark".into(),
        "bootstrap".into(),
    ];
    match command {
        MaintenanceCommand::Rollback => args.push("rollback".into()),
        MaintenanceCommand::RotateLeaf | MaintenanceCommand::RotateCa => {
            args.extend(["cert".into(), "rotate".into()]);
            if command == MaintenanceCommand::RotateCa {
                args.push("--ca".into());
            }
        }
    }
    args.push(if dry_run { "--dry-run" } else { "--yes" }.into());
    args.push("--json".into());
    let output = run_activation_process(&ActivationLaunchSpec {
        program: "ssh",
        args,
        stdin: LaunchStream::Inherit,
        stdout: LaunchStream::Capture,
        stderr: LaunchStream::Capture,
    })
    .map_err(|error| InstallError {
        kind: InstallErrorKind::Unreachable,
        message: format!("start fixed Spark maintenance over SSH: {error}"),
    })?;
    if !output.status.success() {
        return Err(activation_exit_error(
            output.status,
            &output.stderr,
            &output.stdout,
        ));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| configuration_error("maintenance response was not UTF-8"))?
        .replace("\r\n", "\n");
    Ok(text
        .strip_prefix(SUDO_PROMPT)
        .unwrap_or(&text)
        .trim()
        .to_owned())
}

#[cfg(feature = "spark-agent")]
pub fn rollback_installed(dry_run: bool) -> Result<MaintenanceReport, InstallError> {
    rollback_release(Path::new("/"), dry_run, || {
        if dry_run {
            Ok(())
        } else {
            restart_control_plane()
        }
    })
}

#[cfg(feature = "spark-agent")]
pub fn rotate_installed_certificate(
    dry_run: bool,
    rotate_ca: bool,
) -> Result<CertificateRotationReport, InstallError> {
    rotate_certificate(Path::new("/"), dry_run, rotate_ca, || {
        if dry_run {
            Ok(())
        } else {
            require_fixed_success(
                "hot-reload Spark TLS identity",
                "systemctl",
                &["kill", "--signal=HUP", "sy-spark-agent.service"],
            )
        }
    })
}

fn activation_exit_error(
    status: std::process::ExitStatus,
    stderr: &[u8],
    pty_output: &[u8],
) -> InstallError {
    let diagnostic = activation_diagnostic(stderr, pty_output);
    InstallError {
        kind: InstallErrorKind::Unreachable,
        message: format!(
            "fixed Spark activation exited with {status} (remote_diagnostic={diagnostic})"
        ),
    }
}

fn activation_diagnostic(stderr: &[u8], pty_output: &[u8]) -> String {
    let pty = String::from_utf8_lossy(pty_output).replace(SUDO_PROMPT, " ");
    let ssh = String::from_utf8_lossy(stderr);
    let useful_ssh = ssh
        .lines()
        .filter(|line| {
            let line = line.trim();
            !((line.starts_with("Connection to ") || line.starts_with("Shared connection to "))
                && line.contains(" closed"))
        })
        .collect::<Vec<_>>()
        .join(" ");
    sanitize_diagnostic(format!("{pty} {useful_ssh}").as_bytes())
}

struct LocalManifest(PathBuf);

impl LocalManifest {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for LocalManifest {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_local_manifest(path: PathBuf, bytes: &[u8]) -> Result<LocalManifest, InstallError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| {
            configuration_error(format!("stage approved install manifest: {error}"))
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| configuration_error(format!("sync approved install manifest: {error}")))?;
    Ok(LocalManifest(path))
}

fn quote_sftp_path(path: &Path) -> Result<String, InstallError> {
    let value = path
        .to_str()
        .ok_or_else(|| configuration_error("release input path is not UTF-8"))?;
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(configuration_error(
            "release input path contains a control character",
        ));
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn run_release_sftp(host: &str, batch: &str) -> Result<(), InstallError> {
    let mut process = Command::new("sftp")
        .args(["-o", "BatchMode=no", "-b", "-", "--", host])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| InstallError {
            kind: InstallErrorKind::Unreachable,
            message: format!("start release sftp: {error}"),
        })?;
    process
        .stdin
        .take()
        .ok_or_else(|| configuration_error("release sftp stdin unavailable"))?
        .write_all(batch.as_bytes())
        .map_err(|error| InstallError {
            kind: InstallErrorKind::Unreachable,
            message: format!("write release sftp batch: {error}"),
        })?;
    let status = process.wait().map_err(|error| InstallError {
        kind: InstallErrorKind::Unreachable,
        message: format!("wait for release sftp: {error}"),
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(InstallError {
            kind: InstallErrorKind::Unreachable,
            message: format!("release sftp exited with {status}"),
        })
    }
}

fn parse_bootstrap_channel(bytes: &[u8]) -> Result<ActivationResult, InstallError> {
    let mut text = std::str::from_utf8(bytes)
        .map_err(|_| configuration_error("protected SSH bootstrap channel was not UTF-8"))?;
    if let Some(after_prompt) = text.strip_prefix(SUDO_PROMPT) {
        text = after_prompt
            .strip_prefix("\r\n")
            .or_else(|| after_prompt.strip_prefix('\n'))
            .ok_or_else(|| configuration_error("invalid Spark sudo prompt record"))?;
    }
    let fields: Vec<_> = text.lines().collect();
    if fields.len() != 4 {
        return Err(configuration_error(
            "protected SSH bootstrap channel has an incompatible field count",
        ));
    }
    let decode = |value: &str| -> Result<String, InstallError> {
        String::from_utf8(decode_hex(value.trim_end_matches('\r'))?)
            .map_err(|_| configuration_error("protected SSH bootstrap channel field was not UTF-8"))
    };
    let metadata: ActivationMetadata = serde_json::from_str(&decode(fields[0])?)
        .map_err(|error| configuration_error(format!("decode activation metadata: {error}")))?;
    if metadata.schema != "sy.spark.activation-result/v1" {
        return Err(configuration_error("unsupported activation result schema"));
    }
    Ok(ActivationResult {
        changed: metadata.changed,
        preceding_release: metadata.preceding_release.map(PathBuf::from),
        active_release: PathBuf::from(metadata.active_release),
        material: BootstrapMaterial {
            ca_certificate_pem: decode(fields[1])?,
            ca_certificate_sha256: decode(fields[2])?,
            token: decode(fields[3])?,
        },
    })
}

fn is_sha256_text(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(feature = "spark-agent")]
fn validate_release(bundle: &ReleaseBundle<'_>) -> Result<(), InstallError> {
    use minisign_verify::Signature;
    if bundle.version.is_empty()
        || bundle.version.contains('/')
        || bundle.version == "."
        || bundle.version == ".."
    {
        return Err(configuration_error(
            "release version is not a safe path component",
        ));
    }
    let actual = format!("{:x}", Sha256::digest(bundle.executable));
    if actual != bundle.executable_sha256 {
        return Err(configuration_error("release SHA-256 mismatch"));
    }
    let key = decode_release_public_key(bundle.public_key_base64)?;
    let signature = Signature::decode(bundle.signature)
        .map_err(|error| configuration_error(format!("decode release signature: {error}")))?;
    key.verify(bundle.executable, &signature, false)
        .map_err(|error| configuration_error(format!("verify signed ARM64 release: {error}")))
}

#[cfg(feature = "spark-agent")]
fn decode_release_public_key(input: &str) -> Result<minisign_verify::PublicKey, InstallError> {
    use minisign_verify::PublicKey;
    let lines: Vec<_> = input.lines().collect();
    let decoded = match lines.as_slice() {
        [raw] => PublicKey::from_base64(raw),
        [comment, _] if valid_minisign_public_key_comment(comment) => PublicKey::decode(input),
        _ => {
            return Err(configuration_error(
                "invalid minisign public-key file shape",
            ))
        }
    };
    decoded.map_err(|error| configuration_error(format!("decode release public key: {error}")))
}

#[cfg(feature = "spark-agent")]
fn valid_minisign_public_key_comment(comment: &str) -> bool {
    const PREFIXES: [&str; 2] = [
        "untrusted comment: minisign public key ",
        "untrusted comment: minisign public key: ",
    ];
    PREFIXES.iter().any(|prefix| {
        comment.strip_prefix(prefix).is_some_and(|identifier| {
            identifier.len() == 16 && identifier.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    })
}

#[cfg(feature = "spark-agent")]
fn directory_layout() -> [(&'static str, u32, bool); 14] {
    [
        ("opt/sy-spark", 0o755, true),
        ("opt/sy-spark/releases", 0o755, true),
        ("opt/sy-spark/hf-http-fallback", 0o755, true),
        ("etc/sy", 0o750, true),
        ("etc/sy/spark-recipes.d", 0o755, true),
        ("etc/apparmor.d", 0o755, false),
        ("etc/systemd/system", 0o755, false),
        ("var/lib/sy-spark", 0o750, true),
        ("var/lib/sy-spark/huggingface", 0o750, true),
        ("var/lib/sy-spark/compile-cache", 0o750, true),
        ("var/lib/sy-spark/executor", 0o750, true),
        ("var/lib/sy-spark/tls", 0o700, true),
        ("var/lib/sy-spark/ca", 0o700, true),
        ("run/sy-spark", 0o750, true),
    ]
}

#[cfg(feature = "spark-agent")]
fn write_policy_assets(
    root: &Path,
    listen_address: &str,
    service_uid: u32,
    report: &mut InstallReport,
) -> Result<(), InstallError> {
    let config = include_str!("../../configs/sy/spark/agent.toml")
        .replace("10.1.30.143:9843", &format!("{listen_address}:9843"));
    let executor_config = include_str!("../../configs/sy/spark/executor.toml")
        .replace("agent_uid = 996", &format!("agent_uid = {service_uid}"));
    for (relative, bytes, mode) in [
        ("etc/sy/spark-agent.toml", config.as_bytes(), 0o640),
        (
            "etc/sy/spark-executor.toml",
            executor_config.as_bytes(),
            0o640,
        ),
        (
            "etc/systemd/system/sy-spark-agent.service",
            include_bytes!("../../configs/systemd/system/sy-spark-agent.service").as_slice(),
            0o644,
        ),
        (
            "etc/systemd/system/sy-spark-executor.service",
            include_bytes!("../../configs/systemd/system/sy-spark-executor.service").as_slice(),
            0o644,
        ),
        (
            "etc/systemd/system/sy-spark.target",
            include_bytes!("../../configs/systemd/system/sy-spark.target").as_slice(),
            0o644,
        ),
        (
            "etc/apparmor.d/sy-spark-agent",
            include_bytes!("../../configs/apparmor.d/sy-spark-agent").as_slice(),
            0o644,
        ),
        (
            "etc/apparmor.d/sy-spark-executor",
            include_bytes!("../../configs/apparmor.d/sy-spark-executor").as_slice(),
            0o644,
        ),
    ] {
        let path = root.join(relative);
        let differs = !std::fs::read(&path).is_ok_and(|existing| existing == bytes);
        if differs {
            write_synced(&path, bytes, mode, &mut report.fsync_trace)?;
        }
        report
            .actions
            .push(InstallAction::EnsurePolicyAsset(relative.into()));
    }
    std::fs::create_dir_all(root.join("etc/sy/spark-recipes.d")).map_err(|error| {
        configuration_error(format!("create signed recipe catalog directory: {error}"))
    })?;
    for (name, bytes) in crate::spark::recipe::RecipeCatalog::signed_assets() {
        let relative = format!("etc/sy/spark-recipes.d/{name}");
        let path = root.join(&relative);
        if !std::fs::read(&path).is_ok_and(|existing| existing == *bytes) {
            write_synced(&path, bytes, 0o644, &mut report.fsync_trace)?;
        }
        report
            .actions
            .push(InstallAction::EnsurePolicyAsset(relative.into()));
    }
    let fallback_lock = include_bytes!("../../configs/sy/spark/hf-http-fallback.lock");
    let fallback_digest = format!("{:x}", Sha256::digest(fallback_lock));
    let fallback_dir = root
        .join("opt/sy-spark/hf-http-fallback")
        .join(&fallback_digest);
    std::fs::create_dir_all(&fallback_dir).map_err(|error| {
        configuration_error(format!(
            "create hash-locked HTTP fallback directory: {error}"
        ))
    })?;
    let fallback_path = fallback_dir.join("requirements.lock");
    if !std::fs::read(&fallback_path).is_ok_and(|existing| existing == fallback_lock) {
        write_synced(
            &fallback_path,
            fallback_lock,
            0o444,
            &mut report.fsync_trace,
        )?;
    }
    report.actions.push(InstallAction::EnsurePolicyAsset(
        fallback_path
            .strip_prefix(root)
            .unwrap_or(&fallback_path)
            .to_path_buf(),
    ));
    Ok(())
}

#[cfg(feature = "spark-agent")]
fn ensure_local_identity(
    root: &Path,
    address: &str,
    hostname: &str,
    report: &mut InstallReport,
) -> Result<BootstrapMaterial, InstallError> {
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
        IsCa, Issuer, KeyPair, KeyUsagePurpose,
    };
    let ca_cert_path = root.join("var/lib/sy-spark/ca/ca-cert.pem");
    let ca_key_path = root.join("var/lib/sy-spark/ca/ca-key.pem");
    let leaf_cert_path = root.join("var/lib/sy-spark/tls/server-chain.pem");
    let leaf_key_path = root.join("var/lib/sy-spark/tls/server-key.pem");
    let token_path = root.join("etc/sy/spark-bootstrap-admin.credential");
    let hf_token_path = root.join("etc/sy/spark-hf-read.credential");
    if !ca_cert_path.exists() {
        let ca_key = KeyPair::generate()
            .map_err(|error| configuration_error(format!("generate local CA key: {error}")))?;
        let mut ca_params = CertificateParams::new(Vec::<String>::new())
            .map_err(|error| configuration_error(format!("build local CA: {error}")))?;
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "sy Spark local CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = ca_params
            .self_signed(&ca_key)
            .map_err(|error| configuration_error(format!("sign local CA: {error}")))?;
        let leaf_key = KeyPair::generate()
            .map_err(|error| configuration_error(format!("generate leaf key: {error}")))?;
        let mut leaf_params = CertificateParams::new(vec![address.into(), hostname.into()])
            .map_err(|error| configuration_error(format!("build explicit leaf SANs: {error}")))?;
        leaf_params.distinguished_name = DistinguishedName::new();
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, hostname);
        leaf_params.is_ca = IsCa::ExplicitNoCa;
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let issuer = Issuer::from_params(&ca_params, &ca_key);
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &issuer)
            .map_err(|error| configuration_error(format!("sign leaf certificate: {error}")))?;
        write_synced(
            &ca_key_path,
            ca_key.serialize_pem().as_bytes(),
            0o600,
            &mut report.fsync_trace,
        )?;
        write_synced(
            &ca_cert_path,
            ca_cert.pem().as_bytes(),
            0o644,
            &mut report.fsync_trace,
        )?;
        write_synced(
            &leaf_key_path,
            leaf_key.serialize_pem().as_bytes(),
            0o600,
            &mut report.fsync_trace,
        )?;
        write_synced(
            &leaf_cert_path,
            format!("{}{}", leaf_cert.pem(), ca_cert.pem()).as_bytes(),
            0o644,
            &mut report.fsync_trace,
        )?;
        let token = format!(
            "spark-bootstrap.{}.{}.{}",
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4()
        );
        write_synced(
            &token_path,
            token.as_bytes(),
            0o600,
            &mut report.fsync_trace,
        )?;
    }
    if !hf_token_path.exists() {
        write_synced(&hf_token_path, b"", 0o600, &mut report.fsync_trace)?;
    }
    for relative in [
        "var/lib/sy-spark/ca/ca-key.pem",
        "var/lib/sy-spark/ca/ca-cert.pem",
        "var/lib/sy-spark/tls/server-key.pem",
        "var/lib/sy-spark/tls/server-chain.pem",
        "etc/sy/spark-bootstrap-admin.credential",
        "etc/sy/spark-hf-read.credential",
    ] {
        report
            .actions
            .push(InstallAction::EnsureLocalIdentity(relative.into()));
    }
    existing_bootstrap_material(root)
}

#[cfg(feature = "spark-agent")]
fn existing_bootstrap_material(root: &Path) -> Result<BootstrapMaterial, InstallError> {
    let ca_certificate_pem = std::fs::read_to_string(root.join("var/lib/sy-spark/ca/ca-cert.pem"))
        .map_err(|error| configuration_error(format!("read installed CA certificate: {error}")))?;
    let token = std::fs::read_to_string(root.join("etc/sy/spark-bootstrap-admin.credential"))
        .map_err(|error| {
            configuration_error(format!("read installed bootstrap credential: {error}"))
        })?;
    let ca_certificate_sha256 =
        format!("sha256:{:x}", Sha256::digest(ca_certificate_pem.as_bytes()));
    Ok(BootstrapMaterial {
        ca_certificate_pem,
        ca_certificate_sha256,
        token,
    })
}

#[cfg(feature = "spark-agent")]
fn write_synced(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    trace: &mut Vec<PathBuf>,
) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt;
    let mut options = std::fs::OpenOptions::new();
    let mut file = options
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|error| configuration_error(format!("write {}: {error}", path.display())))?;
    file.write_all(bytes)
        .map_err(|error| configuration_error(format!("write {}: {error}", path.display())))?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|error| configuration_error(format!("chmod {}: {error}", path.display())))?;
    file.sync_all()
        .map_err(|error| configuration_error(format!("fsync {}: {error}", path.display())))?;
    trace.push(path.to_path_buf());
    Ok(())
}

#[cfg(feature = "spark-agent")]
fn sync_dir(path: &Path, trace: &mut Vec<PathBuf>) -> Result<(), InstallError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            configuration_error(format!("fsync directory {}: {error}", path.display()))
        })?;
    trace.push(path.to_path_buf());
    Ok(())
}

#[cfg(feature = "spark-agent")]
fn replace_symlink(path: &Path, target: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::symlink;
    let parent = path
        .parent()
        .ok_or_else(|| configuration_error("activation symlink has no parent"))?;
    let staged = parent.join(format!(".link-{}", uuid::Uuid::new_v4()));
    symlink(target, &staged)
        .map_err(|error| configuration_error(format!("stage activation symlink: {error}")))?;
    std::fs::rename(&staged, path)
        .map_err(|error| configuration_error(format!("activate symlink: {error}")))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| configuration_error(format!("fsync activation directory: {error}")))
}

#[cfg(feature = "spark-agent")]
pub fn bootstrap_inventory() -> Result<HostInventory, InstallError> {
    use super::wire::{
        DockerInventory, ExistingInstallation, GpuInventory, OsInventory, PythonInventory,
        INVENTORY_SCHEMA,
    };
    use std::os::unix::net::UnixStream;

    let executable = std::env::current_exe()
        .map_err(|error| configuration_error(format!("resolve bootstrap executable: {error}")))?;
    let probe_sha256 = sha256_file(&executable)?;
    let expected_name = format!("sy-spark-bootstrap-{probe_sha256}");
    if executable.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return Err(configuration_error(
            "bootstrap executable name does not match its SHA-256",
        ));
    }

    let os_release = parse_key_value_file("/etc/os-release")?;
    let gpu_fields = fixed_output(
        "nvidia-smi",
        &[
            "--query-gpu=driver_version,name,compute_cap,vbios_version",
            "--format=csv,noheader,nounits",
        ],
    )?;
    let gpu_parts: Vec<_> = gpu_fields.trim().split(',').map(str::trim).collect();
    if gpu_parts.len() != 4 {
        return Err(configuration_error(
            "nvidia-smi returned an incompatible GPU inventory",
        ));
    }
    let nvidia_summary = fixed_output("nvidia-smi", &[])?;
    let docker_version = extract_version(&fixed_output("docker", &["--version"])?)
        .ok_or_else(|| configuration_error("could not parse the installed Docker version"))?;
    let toolkit_version = fixed_output(
        "dpkg-query",
        &["-W", "-f=${Version}", "nvidia-container-toolkit"],
    )?;
    let systemd_version = fixed_output("systemd", &["--version"])?
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| configuration_error("could not parse systemd version"))?
        .to_string();
    let python_version = fixed_output("python3", &["--version"])?
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| configuration_error("could not parse Python version"))?
        .to_string();
    let current_release = std::fs::read_link("/opt/sy-spark/current")
        .ok()
        .map(|path| path.to_string_lossy().into_owned());

    Ok(HostInventory {
        schema: INVENTORY_SCHEMA.into(),
        probe_sha256,
        hostname: std::fs::read_to_string("/etc/hostname")
            .map_err(|error| configuration_error(format!("read /etc/hostname: {error}")))?
            .trim()
            .into(),
        dgx_software_build: read_dgx_software_build()?,
        os: OsInventory {
            id: required_value(&os_release, "ID")?,
            version_id: required_value(&os_release, "VERSION_ID")?,
            pretty_name: required_value(&os_release, "PRETTY_NAME")?,
        },
        architecture: fixed_output("uname", &["-m"])?.trim().into(),
        kernel_release: fixed_output("uname", &["-r"])?.trim().into(),
        nvidia_driver_version: gpu_parts[0].into(),
        cuda_runtime_version: parse_cuda_version(&nvidia_summary)?,
        firmware_identity: gpu_parts[3].into(),
        gpu: GpuInventory {
            name: gpu_parts[1].into(),
            compute_capability: gpu_parts[2].into(),
        },
        docker: DockerInventory {
            version: docker_version,
            active: fixed_status("systemctl", &["is-active", "--quiet", "docker"]),
            login_user_socket_access: UnixStream::connect("/var/run/docker.sock").is_ok(),
        },
        nvidia_container_toolkit_version: toolkit_version
            .trim()
            .split('-')
            .next()
            .unwrap_or_default()
            .into(),
        systemd_version,
        lsm: inspect_lsm()?,
        python: PythonInventory {
            version: python_version,
            venv_available: fixed_status("python3", &["-m", "venv", "--help"]),
        },
        memory: inspect_memory()?,
        storage: vec![inspect_root_storage()?],
        lan_addresses: inspect_lan_addresses()?,
        existing_installation: ExistingInstallation {
            present: current_release.is_some(),
            current_release,
            state_schema: None,
        },
    })
}

#[cfg(feature = "spark-agent")]
fn fixed_output(program: &str, args: &[&str]) -> Result<String, InstallError> {
    let output = Command::new(program).args(args).output().map_err(|error| {
        configuration_error(format!("run required inspector {program}: {error}"))
    })?;
    if !output.status.success() {
        return Err(configuration_error(format!(
            "required inspector {program} exited with {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| configuration_error(format!("{program} output was not UTF-8: {error}")))
}

#[cfg(feature = "spark-agent")]
fn fixed_status(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(feature = "spark-agent")]
fn parse_key_value_file(
    path: &str,
) -> Result<std::collections::BTreeMap<String, String>, InstallError> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| configuration_error(format!("read {path}: {error}")))?;
    Ok(content
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.into(), value.trim_matches('"').into()))
        .collect())
}

#[cfg(feature = "spark-agent")]
fn required_value(
    values: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> Result<String, InstallError> {
    values
        .get(key)
        .cloned()
        .ok_or_else(|| configuration_error(format!("required host field {key} is absent")))
}

#[cfg(feature = "spark-agent")]
fn read_dgx_software_build() -> Result<String, InstallError> {
    for path in ["/etc/dgx-release", "/etc/nvidia/dgx-release"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Some(version) = extract_version(&content) {
                return Ok(version);
            }
        }
    }
    let package = fixed_output("dpkg-query", &["-W", "-f=${Version}", "dgx-release"])?;
    extract_version(&package)
        .ok_or_else(|| configuration_error("could not identify the DGX software build"))
}

#[cfg(feature = "spark-agent")]
fn extract_version(text: &str) -> Option<String> {
    text.split(|character: char| {
        !(character.is_ascii_alphanumeric() || character == '.' || character == '-')
    })
    .find(|token| token.contains('.') && token.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
    .map(|token| token.trim_end_matches(',').to_string())
}

#[cfg(feature = "spark-agent")]
fn parse_cuda_version(summary: &str) -> Result<String, InstallError> {
    summary
        .split("CUDA Version:")
        .nth(1)
        .and_then(extract_version)
        .ok_or_else(|| configuration_error("could not identify the installed CUDA runtime"))
}

#[cfg(feature = "spark-agent")]
fn inspect_lsm() -> Result<super::wire::LsmInventory, InstallError> {
    let active = std::fs::read_to_string("/sys/kernel/security/lsm").unwrap_or_default();
    let apparmor_enabled = std::fs::read_to_string("/sys/module/apparmor/parameters/enabled")
        .is_ok_and(|value| value.trim().eq_ignore_ascii_case("Y"));
    if apparmor_enabled || active.split(',').any(|name| name.trim() == "apparmor") {
        return Ok(super::wire::LsmInventory {
            kind: "apparmor".into(),
            mode: if fixed_status("aa-status", &["--enabled"]) {
                "enforce".into()
            } else {
                "disabled".into()
            },
        });
    }
    if active.split(',').any(|name| name.trim() == "selinux") {
        return Ok(super::wire::LsmInventory {
            kind: "selinux".into(),
            mode: fixed_output("getenforce", &[])?.trim().to_ascii_lowercase(),
        });
    }
    Ok(super::wire::LsmInventory {
        kind: "none".into(),
        mode: "disabled".into(),
    })
}

#[cfg(feature = "spark-agent")]
fn inspect_memory() -> Result<super::wire::MemoryInventory, InstallError> {
    let content = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| configuration_error(format!("read /proc/meminfo: {error}")))?;
    let values: std::collections::BTreeMap<String, String> = content
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.into(), value.trim().into()))
        .collect();
    let kib = |key| -> Result<u64, InstallError> {
        required_value(&values, key)?
            .split_whitespace()
            .next()
            .ok_or_else(|| configuration_error(format!("invalid /proc/meminfo field {key}")))?
            .parse::<u64>()
            .map_err(|error| {
                configuration_error(format!("invalid /proc/meminfo field {key}: {error}"))
            })
    };
    Ok(super::wire::MemoryInventory {
        total_bytes: kib("MemTotal")? * 1024,
        available_bytes: kib("MemAvailable")? * 1024,
        swap_total_bytes: kib("SwapTotal")? * 1024,
    })
}

#[cfg(feature = "spark-agent")]
fn inspect_root_storage() -> Result<super::wire::StorageInventory, InstallError> {
    let output = fixed_output(
        "df",
        &["-B1", "--output=source,fstype,size,avail,target", "/"],
    )?;
    let fields: Vec<_> = output
        .lines()
        .last()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    if fields.len() != 5 {
        return Err(configuration_error(
            "df returned an incompatible root filesystem inventory",
        ));
    }
    Ok(super::wire::StorageInventory {
        source: fields[0].into(),
        filesystem: fields[1].into(),
        total_bytes: fields[2]
            .parse()
            .map_err(|error| configuration_error(format!("invalid df size: {error}")))?,
        free_bytes: fields[3]
            .parse()
            .map_err(|error| configuration_error(format!("invalid df free space: {error}")))?,
        mount_point: fields[4].into(),
    })
}

#[cfg(feature = "spark-agent")]
fn inspect_lan_addresses() -> Result<Vec<String>, InstallError> {
    let value: serde_json::Value = serde_json::from_str(&fixed_output(
        "ip",
        &["-j", "-4", "address", "show", "up", "scope", "global"],
    )?)
    .map_err(|error| configuration_error(format!("decode ip address inventory: {error}")))?;
    let mut addresses = Vec::new();
    for interface in value.as_array().into_iter().flatten() {
        let name = interface["ifname"].as_str().unwrap_or_default();
        if name.starts_with("docker") || name.starts_with("br-") || name.starts_with("veth") {
            continue;
        }
        for address in interface["addr_info"].as_array().into_iter().flatten() {
            if address["family"] == "inet" && address["scope"] == "global" {
                if let Some(local) = address["local"].as_str() {
                    addresses.push(local.into());
                }
            }
        }
    }
    addresses.sort();
    addresses.dedup();
    Ok(addresses)
}

fn validate_remote_probe_path(path: &str) -> Result<(), String> {
    const PREFIX: &str = "/tmp/sy-spark-bootstrap-";
    let digest = path
        .strip_prefix(PREFIX)
        .ok_or_else(|| "bootstrap probe path is outside the application prefix".to_string())?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("bootstrap probe path is not content-addressed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_manifest, BootstrapRunner, InspectInvocation, OpenSshRunner, PlanOptions,
        ProbeTransfer, RunnerError,
    };
    use crate::spark::wire::{decode_inventory, AssetKind, ContentIdentity};
    use std::path::PathBuf;

    #[cfg(feature = "spark-agent")]
    fn signed_bundle<'a>(
        executable: &'a [u8],
        public_key: &'a str,
        signature: &'a str,
        sha256: &'a str,
    ) -> super::ReleaseBundle<'a> {
        super::ReleaseBundle {
            version: "0.1.0",
            executable,
            executable_sha256: sha256,
            public_key_base64: public_key,
            signature,
            listen_address: "127.0.0.1",
            hostname: "spark.test",
            active_lsm: "apparmor:enforce",
        }
    }

    #[test]
    fn host_alias_and_paths_are_discrete_argv() {
        let transfer = ProbeTransfer {
            host_alias: "spark alias; touch /tmp/pwned".into(),
            local_path: PathBuf::from("/tmp/probe path;echo pwned"),
            remote_path: format!("/tmp/sy-spark-bootstrap-{}", "a".repeat(64)),
        };
        let upload = OpenSshRunner::upload_process(&transfer).unwrap();
        let upload_args: Vec<_> = upload.get_args().collect();
        assert_eq!(upload_args[5], transfer.host_alias.as_str());
        let batch = OpenSshRunner::sftp_batch(&transfer, false).unwrap();
        assert!(batch.ends_with(&format!("chmod 0700 {}\n", transfer.remote_path)));

        let invocation = InspectInvocation::from_transfer(&transfer);
        let inspect = OpenSshRunner::inspect_process(&invocation);
        let inspect_args: Vec<_> = inspect.get_args().collect();
        assert_eq!(inspect_args[1], transfer.host_alias.as_str());
        assert_eq!(inspect_args[2], transfer.remote_path.as_str());
        assert_eq!(&inspect_args[3..], ["spark", "bootstrap", "inspect"]);
    }

    #[test]
    fn sftp_batch_explicitly_allows_openssh_interactive_authentication() {
        let transfer = ProbeTransfer {
            host_alias: "dgx-spark".into(),
            local_path: PathBuf::from("/tmp/probe"),
            remote_path: format!("/tmp/sy-spark-bootstrap-{}", "a".repeat(64)),
        };
        let process = OpenSshRunner::upload_process(&transfer).unwrap();
        let args: Vec<_> = process.get_args().collect();
        assert_eq!(args, ["-o", "BatchMode=no", "-b", "-", "--", "dgx-spark"]);
    }

    #[test]
    fn activation_process_inherits_terminal_input_and_captures_only_protocol_output() {
        const PREFIX: &str = "/tmp/sy-spark-release-abc";
        let signature = format!("{PREFIX}.minisig");
        let public_key = format!("{PREFIX}.pub");
        let manifest = format!("{PREFIX}.manifest.json");
        let spec = super::activation_launch_spec(super::ActivationLaunchInput {
            host: "dgx-spark",
            prefix: PREFIX,
            signature: &signature,
            public_key: &public_key,
            manifest: &manifest,
            manifest_sha256: "manifest-sha256",
            version: "0.1.0",
            listen_address: "10.1.30.143",
            hostname: "dgx-spark",
            active_lsm: "apparmor:enforce",
        });
        assert_eq!(spec.stdin, super::LaunchStream::Inherit);
        assert_eq!(spec.stdout, super::LaunchStream::Capture);
        assert_eq!(spec.stderr, super::LaunchStream::Capture);
        assert_eq!(spec.program, "ssh");
        assert_eq!(spec.args[5], super::SUDO_PROMPT);
        assert_eq!(
            spec.args,
            [
                "-tt",
                "--",
                "dgx-spark",
                "sudo",
                "-p",
                super::SUDO_PROMPT,
                "--",
                PREFIX,
                "spark",
                "bootstrap",
                "activate",
                "--executable",
                PREFIX,
                "--signature",
                &signature,
                "--public-key",
                &public_key,
                "--manifest",
                &manifest,
                "--manifest-sha256",
                "manifest-sha256",
                "--version",
                "0.1.0",
                "--listen-address",
                "10.1.30.143",
                "--hostname",
                "dgx-spark",
                "--active-lsm",
                "apparmor:enforce",
            ]
        );
    }

    #[test]
    fn activation_stages_the_executable_under_its_bootstrap_identity() {
        let sha256 = "a".repeat(64);
        assert_eq!(
            super::activation_remote_prefix(&sha256).unwrap(),
            format!("/tmp/sy-spark-bootstrap-{sha256}")
        );
    }

    #[test]
    #[cfg(feature = "spark-agent")]
    fn host_integration_starts_the_enabled_supervision_target() {
        assert_eq!(
            super::target_enable_args(),
            ["enable", "--now", "sy-spark.target"]
        );
    }

    #[test]
    fn diagnostics_are_redacted_single_line_and_bounded() {
        let secret = "spark-bootstrap.secret-material";
        let input = format!(
            "failed\nAuthorization: Bearer {secret} token={secret} {{\"manifest\":\"{secret}\"}} {}",
            "x".repeat(2_000)
        );
        let output = super::sanitize_diagnostic(input.as_bytes());
        assert!(!output.contains(secret));
        assert!(!output.contains('\n'));
        assert!(output.len() <= super::DIAGNOSTIC_LIMIT + " [truncated]".len());
    }

    #[test]
    fn remote_activation_failure_preserves_only_sanitized_stderr() {
        use std::os::unix::process::ExitStatusExt;
        let error = super::activation_exit_error(
            std::process::ExitStatus::from_raw(2 << 8),
            b"systemctl failed for token=remote-secret\nunit not ready",
            b"ignored protected output",
        );
        assert!(error.message.contains("exit status: 2"));
        assert!(error.message.contains("systemctl failed"));
        assert!(error.message.contains("unit not ready"));
        assert!(!error.message.contains("remote-secret"));
        assert!(!error.message.contains('\n'));

        let pty_error = super::activation_exit_error(
            std::process::ExitStatus::from_raw(2 << 8),
            b"",
            b"pty merged failure for credential=pty-secret",
        );
        assert!(pty_error.message.contains("pty merged failure"));
        assert!(!pty_error.message.contains("pty-secret"));
    }

    #[test]
    fn activation_failure_keeps_pty_action_and_drops_ssh_close_noise() {
        use std::os::unix::process::ExitStatusExt;
        let error = super::activation_exit_error(
            std::process::ExitStatus::from_raw(2 << 8),
            b"Connection to dgx-spark closed.\n",
            b"fixed host-integration action enable Spark services failed token=secret",
        );
        assert!(error.message.contains("enable Spark services"));
        assert!(!error.message.contains("Connection to"));
        assert!(!error.message.contains("secret"));
    }

    #[test]
    fn activation_stdout_surfaces_only_the_exact_sudo_prompt() {
        let input = format!("{}\r\nprotected-protocol", super::SUDO_PROMPT);
        let mut visible = Vec::new();
        let captured =
            super::capture_activation_stdout(std::io::Cursor::new(input.as_bytes()), &mut visible)
                .unwrap();
        assert_eq!(visible, super::SUDO_PROMPT.as_bytes());
        assert_eq!(captured, input.as_bytes());
        assert!(!visible.windows(8).any(|bytes| bytes == b"protocol"));
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn fixed_action_failure_names_the_exact_allowlisted_action() {
        let error = super::require_fixed_success(
            "reload executor confinement",
            "sh",
            &[
                "-c",
                "printf 'token=secret-value deterministic failure' >&2; exit 7",
            ],
        )
        .unwrap_err();
        assert!(error.message.contains("reload executor confinement"));
        assert!(error.message.contains("program=sh"));
        assert!(error.message.contains("status=7"));
        assert!(error.message.contains("deterministic failure"));
        assert!(!error.message.contains("secret-value"));
    }

    struct TypeCheckedRunner;

    impl BootstrapRunner for TypeCheckedRunner {
        fn upload_probe(&self, _: &ProbeTransfer) -> Result<(), RunnerError> {
            Ok(())
        }

        fn inspect(&self, _: &InspectInvocation) -> Result<Vec<u8>, RunnerError> {
            Ok(Vec::new())
        }

        fn remove_probe(&self, _: &ProbeTransfer) -> Result<(), RunnerError> {
            Ok(())
        }
    }

    #[test]
    fn runner_has_no_password_or_arbitrary_command_input() {
        fn accepts_typed_runner(_: &dyn BootstrapRunner) {}
        accepts_typed_runner(&TypeCheckedRunner);
        let source = include_str!("install.rs");
        assert!(!source.contains(concat!("pass", "word:")));
        assert!(!source.contains(concat!("caller_", "command")));
        assert!(!source.contains(concat!("ssh", "pass")));
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn capability_probe_output_is_not_written_to_wire_stream() {
        const REENTRY: &str = "SY_SPARK_STATUS_PROBE_TEST";
        const MARKER: &str = "spark-capability-probe-output";
        if std::env::var_os(REENTRY).is_some() {
            assert!(super::fixed_status(
                "sh",
                &["-c", "printf spark-capability-probe-output; printf spark-capability-probe-output >&2"]
            ));
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "spark::install::tests::capability_probe_output_is_not_written_to_wire_stream",
                "--nocapture",
            ])
            .env(REENTRY, "1")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains(MARKER));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(MARKER));
    }

    #[test]
    fn manifest_is_complete_stable_and_read_only() {
        let inventory = decode_inventory(super::tests_fixture::INVENTORY.as_bytes()).unwrap();
        let opts = PlanOptions {
            host_alias: "dgx-spark".into(),
            listen_address: "10.1.30.143".into(),
            listen_port: 9843,
            probe_remote_path: format!("/tmp/sy-spark-bootstrap-{}", "a".repeat(64)),
            probe_sha256: "a".repeat(64),
            probe_removed: true,
        };
        let manifest = build_manifest(inventory, opts).unwrap();
        let paths: Vec<_> = manifest
            .assets
            .iter()
            .map(|asset| asset.path_or_name.as_str())
            .collect();
        let release_path = format!(
            "/opt/sy-spark/releases/{}-{}",
            env!("CARGO_PKG_VERSION"),
            "a".repeat(64)
        );
        assert!(paths.contains(&release_path.as_str()));
        assert!(paths.contains(&format!("{release_path}/sy").as_str()));
        assert!(paths.contains(&"/opt/sy-spark/current"));
        assert!(paths.contains(&"/var/lib/sy-spark/state.sqlite3"));
        assert!(paths.contains(&"/etc/systemd/system/sy-spark-agent.service"));
        assert!(paths.contains(&"/etc/systemd/system/sy-spark-executor.service"));
        assert!(paths.contains(&"/etc/systemd/system/sy-spark.target"));
        assert!(manifest
            .assets
            .iter()
            .any(|asset| asset.kind == AssetKind::Identity));
        assert!(manifest
            .assets
            .iter()
            .any(|asset| asset.kind == AssetKind::Certificate));
        assert!(manifest
            .assets
            .iter()
            .any(|asset| asset.kind == AssetKind::Credential));
        assert!(manifest
            .assets
            .iter()
            .any(|asset| asset.kind == AssetKind::Recipe));
        assert!(manifest
            .assets
            .iter()
            .any(|asset| matches!(asset.content, ContentIdentity::Sha256(_))));
        assert_eq!(manifest.rejected_updates.len(), 17);
        assert!(!manifest.installation_performed);
        assert!(manifest.protected_versions_must_remain_unchanged);
        assert_eq!(
            serde_json::to_vec(&manifest).unwrap(),
            serde_json::to_vec(&manifest).unwrap()
        );
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn first_install_then_reapply_is_atomic_and_idempotent() {
        use minisign::KeyPair;
        use sha2::{Digest, Sha256};
        use std::{io::Cursor, os::unix::fs::PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let executable = b"signed-aarch64-sy-release";
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keys.pk.to_base64();
        let signature = minisign::sign(None, &keys.sk, Cursor::new(executable), None, None)
            .unwrap()
            .into_string();
        let sha256 = format!("{:x}", Sha256::digest(executable));
        let bundle = signed_bundle(executable, &public_key, &signature, &sha256);
        let release_name = format!("0.1.0-{sha256}");
        let (first, first_material) = super::install_release(root.path(), &bundle).unwrap();
        assert!(first.changed);
        assert_eq!(
            std::fs::read(
                root.path()
                    .join("opt/sy-spark/releases")
                    .join(&release_name)
                    .join("sy")
            )
            .unwrap(),
            executable
        );
        assert_eq!(
            std::fs::read_link(root.path().join("opt/sy-spark/current")).unwrap(),
            PathBuf::from("releases").join(&release_name)
        );
        assert_eq!(
            std::fs::metadata(root.path().join("etc/sy/spark-bootstrap-admin.credential"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let file_sync = first
            .fsync_trace
            .iter()
            .position(|path| {
                path.to_string_lossy()
                    .contains(&format!(".stage-{release_name}-"))
                    && path.ends_with("sy")
            })
            .unwrap();
        let stage_sync = first
            .fsync_trace
            .iter()
            .position(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with(&format!(".stage-{release_name}-"))
                })
            })
            .unwrap();
        let release_parent_sync = first
            .fsync_trace
            .iter()
            .position(|path| path.ends_with("opt/sy-spark/releases"))
            .unwrap();
        assert!(file_sync < stage_sync && stage_sync < release_parent_sync);
        assert!(first
            .fsync_trace
            .iter()
            .any(|path| path.ends_with("opt/sy-spark")));
        let (second, second_material) = super::install_release(root.path(), &bundle).unwrap();
        assert!(!second.changed);
        assert!(second.actions.is_empty() && second.fsync_trace.is_empty());
        assert_eq!(
            first_material.ca_certificate_sha256,
            second_material.ca_certificate_sha256
        );
        assert_eq!(first_material.token, second_material.token);
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn same_version_different_signed_artifacts_coexist_without_overwrite() {
        use minisign::KeyPair;
        use sha2::{Digest, Sha256};
        use std::io::Cursor;

        let root = tempfile::tempdir().unwrap();
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let first_bytes = b"first-signed-aarch64-release";
        let first_hash = format!("{:x}", Sha256::digest(first_bytes));
        let first_signature = minisign::sign(None, &keys.sk, Cursor::new(first_bytes), None, None)
            .unwrap()
            .into_string();
        let public_key = keys.pk.to_base64();
        let first = signed_bundle(first_bytes, &public_key, &first_signature, &first_hash);
        super::install_release(root.path(), &first).unwrap();

        let second_bytes = b"second-signed-aarch64-release";
        let second_hash = format!("{:x}", Sha256::digest(second_bytes));
        let second_signature =
            minisign::sign(None, &keys.sk, Cursor::new(second_bytes), None, None)
                .unwrap()
                .into_string();
        let second = signed_bundle(second_bytes, &public_key, &second_signature, &second_hash);
        let (installed, _) = super::install_release(root.path(), &second).unwrap();

        let first_name = format!("0.1.0-{first_hash}");
        let second_name = format!("0.1.0-{second_hash}");
        assert_eq!(
            std::fs::read(
                root.path()
                    .join("opt/sy-spark/releases")
                    .join(&first_name)
                    .join("sy")
            )
            .unwrap(),
            first_bytes
        );
        assert_eq!(
            std::fs::read(
                root.path()
                    .join("opt/sy-spark/releases")
                    .join(&second_name)
                    .join("sy")
            )
            .unwrap(),
            second_bytes
        );
        assert_eq!(
            std::fs::read_link(root.path().join("opt/sy-spark/current")).unwrap(),
            PathBuf::from("releases").join(second_name)
        );
        assert_eq!(
            std::fs::read_link(root.path().join("opt/sy-spark/previous")).unwrap(),
            PathBuf::from("releases").join(first_name)
        );
        assert!(installed.changed);
        assert!(
            !super::install_release(root.path(), &second)
                .unwrap()
                .0
                .changed
        );
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn rollback_swaps_only_control_plane_release_links() {
        use sha2::Digest;
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let releases = root.path().join("opt/sy-spark/releases");
        std::fs::create_dir_all(&releases).unwrap();
        let first_bytes = b"first-control-plane";
        let second_bytes = b"second-control-plane";
        let first_hash = format!("{:x}", sha2::Sha256::digest(first_bytes));
        let second_hash = format!("{:x}", sha2::Sha256::digest(second_bytes));
        let first = format!("0.1.0-{first_hash}");
        let second = format!("0.1.0-{second_hash}");
        for (name, bytes) in [
            (&first, first_bytes.as_slice()),
            (&second, second_bytes.as_slice()),
        ] {
            std::fs::create_dir(releases.join(name)).unwrap();
            std::fs::write(releases.join(name).join("sy"), bytes).unwrap();
        }
        symlink(
            PathBuf::from("releases").join(&second),
            root.path().join("opt/sy-spark/current"),
        )
        .unwrap();
        symlink(
            PathBuf::from("releases").join(&first),
            root.path().join("opt/sy-spark/previous"),
        )
        .unwrap();

        let report = super::rollback_release(root.path(), false, || Ok(())).unwrap();

        assert!(report.applied);
        assert_eq!(
            std::fs::read_link(root.path().join("opt/sy-spark/current")).unwrap(),
            PathBuf::from("releases").join(first)
        );
        assert_eq!(report.docker_restart, "not_run");
        assert_eq!(report.host_reboot, "not_run");
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn rollback_rejects_parent_directory_release_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("opt/sy-spark")).unwrap();
        symlink("releases/..", root.path().join("opt/sy-spark/current")).unwrap();
        let error =
            super::checked_release_link(root.path(), &root.path().join("opt/sy-spark/current"))
                .unwrap_err();
        assert!(error.to_string().contains("escapes the release directory"));
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn generated_ca_and_leaf_have_verifiable_server_certificate_extensions() {
        use minisign::KeyPair;
        use sha2::{Digest, Sha256};
        use std::io::Cursor;
        use x509_parser::{extensions::GeneralName, parse_x509_certificate, pem::parse_x509_pem};

        let executable = b"signed-aarch64-sy-release";
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keys.pk.to_base64();
        let signature = minisign::sign(None, &keys.sk, Cursor::new(executable), None, None)
            .unwrap()
            .into_string();
        let sha256 = format!("{:x}", Sha256::digest(executable));
        let bundle = signed_bundle(executable, &public_key, &signature, &sha256);
        let root = tempfile::tempdir().unwrap();
        super::install_release(root.path(), &bundle).unwrap();

        let ca_pem = std::fs::read(root.path().join("var/lib/sy-spark/ca/ca-cert.pem")).unwrap();
        let leaf_pem =
            std::fs::read(root.path().join("var/lib/sy-spark/tls/server-chain.pem")).unwrap();
        let (_, ca_pem) = parse_x509_pem(&ca_pem).unwrap();
        let (_, leaf_pem) = parse_x509_pem(&leaf_pem).unwrap();
        let (_, ca) = parse_x509_certificate(&ca_pem.contents).unwrap();
        let (_, leaf) = parse_x509_certificate(&leaf_pem.contents).unwrap();
        assert!(ca.basic_constraints().unwrap().unwrap().value.ca);
        assert!(ca.key_usage().unwrap().unwrap().value.key_cert_sign());
        assert!(!leaf.basic_constraints().unwrap().unwrap().value.ca);
        assert!(leaf.key_usage().unwrap().unwrap().value.digital_signature());
        assert!(
            leaf.extended_key_usage()
                .unwrap()
                .unwrap()
                .value
                .server_auth
        );
        let sans = &leaf
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names;
        assert!(sans.contains(&GeneralName::DNSName("spark.test")));
        assert!(sans.contains(&GeneralName::IPAddress(&[127, 0, 0, 1])));
        assert_eq!(leaf.issuer(), ca.subject());
        assert_ne!(leaf.subject(), leaf.issuer());
        leaf.verify_signature(Some(ca.public_key())).unwrap();
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn certificate_rotation_preserves_overlap_and_requires_repin_only_for_ca() {
        use minisign::KeyPair;
        use sha2::{Digest, Sha256};
        use std::io::Cursor;

        let executable = b"signed-aarch64-sy-release";
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keys.pk.to_base64();
        let signature = minisign::sign(None, &keys.sk, Cursor::new(executable), None, None)
            .unwrap()
            .into_string();
        let sha256 = format!("{:x}", Sha256::digest(executable));
        let bundle = signed_bundle(executable, &public_key, &signature, &sha256);
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        std::fs::write(root.path().join("etc/hostname"), "spark.test\n").unwrap();
        super::install_release(root.path(), &bundle).unwrap();
        let original_ca =
            std::fs::read(root.path().join("var/lib/sy-spark/ca/ca-cert.pem")).unwrap();
        let original_leaf =
            std::fs::read(root.path().join("var/lib/sy-spark/tls/server-chain.pem")).unwrap();

        let leaf = super::rotate_certificate(root.path(), false, false, || Ok(())).unwrap();
        assert!(leaf.applied && !leaf.client_repin_required);
        assert_eq!(
            std::fs::read(root.path().join("var/lib/sy-spark/ca/ca-cert.pem")).unwrap(),
            original_ca
        );
        assert_eq!(
            std::fs::read(
                root.path()
                    .join("var/lib/sy-spark/tls/server-chain.pem.overlap")
            )
            .unwrap(),
            original_leaf
        );

        let ca = super::rotate_certificate(root.path(), false, true, || Ok(())).unwrap();
        assert!(ca.applied && ca.client_repin_required && ca.ca_certificate_pem.is_some());
        assert_ne!(
            std::fs::read(root.path().join("var/lib/sy-spark/ca/ca-cert.pem")).unwrap(),
            original_ca
        );
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn release_verification_accepts_raw_and_ordinary_minisign_public_keys() {
        use minisign::KeyPair;
        use sha2::{Digest, Sha256};
        use std::io::Cursor;

        let executable = b"signed-aarch64-sy-release";
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let raw = keys.pk.to_base64();
        let public_key_file = keys.pk.to_box().unwrap().to_string();
        let signature = minisign::sign(None, &keys.sk, Cursor::new(executable), None, None)
            .unwrap()
            .into_string();
        let sha256 = format!("{:x}", Sha256::digest(executable));
        for public_key in [&raw, &public_key_file] {
            let bundle = signed_bundle(executable, public_key, &signature, &sha256);
            super::validate_release(&bundle).unwrap();
        }
        let contaminated = format!("{public_key_file}extra\n");
        let bundle = signed_bundle(executable, &contaminated, &signature, &sha256);
        assert!(super::validate_release(&bundle).is_err());
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn protected_update_commands_are_unrepresentable() {
        fn is_application_owned(action: &super::InstallAction) -> bool {
            match action {
                super::InstallAction::EnsureIdentity(_)
                | super::InstallAction::CreateDirectory(_)
                | super::InstallAction::EnsureReleaseDirectory(_)
                | super::InstallAction::VerifyReleaseArtifact(_)
                | super::InstallAction::EnsurePolicyAsset(_)
                | super::InstallAction::EnsureLocalIdentity(_)
                | super::InstallAction::ActivateRelease(_)
                | super::InstallAction::ApplyServiceTransition(_) => true,
            }
        }
        assert!(is_application_owned(&super::InstallAction::EnsureIdentity(
            "user:sy-spark".into()
        )));
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn approved_install_assets_and_executed_actions_are_one_to_one() {
        use crate::spark::wire::{Applicability, ExecutionPhase};
        use minisign::KeyPair;
        use sha2::{Digest, Sha256};
        use std::io::Cursor;

        let executable = b"signed-aarch64-sy-release";
        let sha256 = format!("{:x}", Sha256::digest(executable));
        let mut inventory = decode_inventory(super::tests_fixture::INVENTORY.as_bytes()).unwrap();
        inventory.probe_sha256.clone_from(&sha256);
        let manifest = build_manifest(
            inventory,
            PlanOptions {
                host_alias: "dgx-spark".into(),
                listen_address: "10.1.30.143".into(),
                listen_port: 9843,
                probe_remote_path: format!("/tmp/sy-spark-bootstrap-{sha256}"),
                probe_sha256: sha256.clone(),
                probe_removed: true,
            },
        )
        .unwrap();
        assert!(manifest.assets.iter().any(|asset| {
            asset.path_or_name.contains("sy-spark-executor")
                && asset.applicability
                    == Applicability::ApplyNow {
                        phase: ExecutionPhase::RemoteInstall,
                    }
        }));
        assert!(manifest.assets.iter().any(|asset| {
            asset.path_or_name == "local-config:spark/<host>"
                && asset.applicability
                    == Applicability::ApplyNow {
                        phase: ExecutionPhase::LocalCredentialStore,
                    }
        }));

        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keys.pk.to_base64();
        let signature = minisign::sign(None, &keys.sk, Cursor::new(executable), None, None)
            .unwrap()
            .into_string();
        let bundle = signed_bundle(executable, &public_key, &signature, &sha256);
        let root = tempfile::tempdir().unwrap();
        let (report, _) = super::install_release(root.path(), &bundle).unwrap();
        super::validate_install_actions(&manifest, &report.actions).unwrap();
        let mut hidden_duplicate = report.actions.clone();
        hidden_duplicate.push(report.actions[0].clone());
        assert!(super::validate_install_actions(&manifest, &hidden_duplicate).is_err());
        assert!(super::validate_install_actions(
            &manifest,
            &report.actions[..report.actions.len() - 1]
        )
        .is_err());
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn activation_response_is_strict_reports_no_change_and_redacts_token() {
        use minisign::KeyPair;
        use sha2::{Digest, Sha256};
        use std::io::Cursor;

        let executable = b"signed-aarch64-sy-release";
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keys.pk.to_base64();
        let signature = minisign::sign(None, &keys.sk, Cursor::new(executable), None, None)
            .unwrap()
            .into_string();
        let sha256 = format!("{:x}", Sha256::digest(executable));
        let bundle = signed_bundle(executable, &public_key, &signature, &sha256);
        let root = tempfile::tempdir().unwrap();
        let (first, material) = super::install_release(root.path(), &bundle).unwrap();
        let mut first_wire = Vec::new();
        super::write_bootstrap_channel(&first, &material, &mut first_wire).unwrap();
        let first_result = super::parse_bootstrap_channel(&first_wire).unwrap();
        assert!(first_result.changed);
        assert!(!format!("{first_result:?}").contains(&material.token));

        let (second, second_material) = super::install_release(root.path(), &bundle).unwrap();
        let mut second_wire = Vec::new();
        super::write_bootstrap_channel(&second, &second_material, &mut second_wire).unwrap();
        let second_result = super::parse_bootstrap_channel(&second_wire).unwrap();
        assert!(!second_result.changed);
        second_wire.extend_from_slice(b"00\n");
        assert!(super::parse_bootstrap_channel(&second_wire).is_err());
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn activation_prompt_is_exact_and_contamination_is_rejected() {
        let report = super::InstallReport {
            changed: true,
            actions: Vec::new(),
            fsync_trace: Vec::new(),
            preceding_release: None,
            active_release: PathBuf::from("opt/sy-spark/releases/0.1.0"),
        };
        let material = super::BootstrapMaterial {
            ca_certificate_pem: "test-ca".into(),
            ca_certificate_sha256: "sha256:test-pin".into(),
            token: "test-token".into(),
        };
        let mut frames = Vec::new();
        super::write_bootstrap_channel(&report, &material, &mut frames).unwrap();
        assert!(super::parse_bootstrap_channel(&frames).is_ok());

        let prompt = format!("{}\n", super::SUDO_PROMPT);
        let prompted = [prompt.as_bytes(), frames.as_slice()].concat();
        assert!(super::parse_bootstrap_channel(&prompted).is_ok());
        assert!(super::parse_bootstrap_channel(
            &[prompt.as_bytes(), prompt.as_bytes(), frames.as_slice()].concat()
        )
        .is_err());
        assert!(super::parse_bootstrap_channel(
            &[b"unexpected\n".as_slice(), frames.as_slice()].concat()
        )
        .is_err());
        let first_end = frames.iter().position(|byte| *byte == b'\n').unwrap() + 1;
        assert!(super::parse_bootstrap_channel(
            &[
                &frames[..first_end],
                prompt.as_bytes(),
                &frames[first_end..],
            ]
            .concat()
        )
        .is_err());
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn integration_failure_removes_first_install_transaction_residue() {
        use minisign::KeyPair;
        use sha2::{Digest, Sha256};
        use std::io::Cursor;

        let executable = b"signed-aarch64-sy-release";
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keys.pk.to_base64();
        let signature = minisign::sign(None, &keys.sk, Cursor::new(executable), None, None)
            .unwrap()
            .into_string();
        let sha256 = format!("{:x}", Sha256::digest(executable));
        let bundle = signed_bundle(executable, &public_key, &signature, &sha256);
        let release_path = format!("opt/sy-spark/releases/0.1.0-{sha256}");
        let root = tempfile::tempdir().unwrap();
        let result = super::install_release_with_integration(root.path(), &bundle, || {
            Err(super::configuration_error("injected integration failure"))
        });
        assert!(result.is_err());
        for relative in [
            "opt/sy-spark/current",
            &release_path,
            "etc/sy/spark-agent.toml",
            "etc/sy/spark-executor.toml",
            "etc/systemd/system/sy-spark-agent.service",
            "etc/systemd/system/sy-spark-executor.service",
            "etc/apparmor.d/sy-spark-agent",
            "etc/apparmor.d/sy-spark-executor",
            "etc/sy/spark-recipes.d/ornith-vllm.toml",
            "etc/sy/spark-recipes.d/qwen3-embedding.toml",
            "etc/sy/spark-bootstrap-admin.credential",
            "var/lib/sy-spark/ca/ca-key.pem",
            "var/lib/sy-spark/tls/server-key.pem",
        ] {
            assert!(!root.path().join(relative).exists(), "residue: {relative}");
        }
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn failed_reapply_restores_prior_activation_config_and_policy() {
        use minisign::KeyPair;
        use sha2::{Digest, Sha256};
        use std::io::Cursor;

        let executable = b"signed-aarch64-sy-release";
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keys.pk.to_base64();
        let signature = minisign::sign(None, &keys.sk, Cursor::new(executable), None, None)
            .unwrap()
            .into_string();
        let sha256 = format!("{:x}", Sha256::digest(executable));
        let first = signed_bundle(executable, &public_key, &signature, &sha256);
        let first_release = format!("0.1.0-{sha256}");
        let root = tempfile::tempdir().unwrap();
        super::install_release(root.path(), &first).unwrap();
        for (relative, prior) in [
            ("etc/sy/spark-agent.toml", b"prior-config".as_slice()),
            (
                "etc/sy/spark-executor.toml",
                b"prior-executor-config".as_slice(),
            ),
            (
                "etc/systemd/system/sy-spark-agent.service",
                b"prior-unit".as_slice(),
            ),
            (
                "etc/systemd/system/sy-spark-executor.service",
                b"prior-executor-unit".as_slice(),
            ),
            ("etc/apparmor.d/sy-spark-agent", b"prior-policy".as_slice()),
            (
                "etc/apparmor.d/sy-spark-executor",
                b"prior-executor-policy".as_slice(),
            ),
            (
                "etc/sy/spark-recipes.d/ornith-vllm.toml",
                b"prior-ornith-recipe".as_slice(),
            ),
            (
                "etc/sy/spark-recipes.d/qwen3-embedding.toml",
                b"prior-embedding-recipe".as_slice(),
            ),
        ] {
            std::fs::write(root.path().join(relative), prior).unwrap();
        }
        let mut update = signed_bundle(executable, &public_key, &signature, &sha256);
        update.version = "0.2.0";
        let result = super::install_release_with_integration(root.path(), &update, || {
            Err(super::configuration_error("injected integration failure"))
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_link(root.path().join("opt/sy-spark/current")).unwrap(),
            PathBuf::from("releases").join(first_release)
        );
        assert!(!root
            .path()
            .join("opt/sy-spark/releases")
            .join(format!("0.2.0-{sha256}"))
            .exists());
        for (relative, prior) in [
            ("etc/sy/spark-agent.toml", b"prior-config".as_slice()),
            (
                "etc/sy/spark-executor.toml",
                b"prior-executor-config".as_slice(),
            ),
            (
                "etc/systemd/system/sy-spark-agent.service",
                b"prior-unit".as_slice(),
            ),
            (
                "etc/systemd/system/sy-spark-executor.service",
                b"prior-executor-unit".as_slice(),
            ),
            ("etc/apparmor.d/sy-spark-agent", b"prior-policy".as_slice()),
            (
                "etc/apparmor.d/sy-spark-executor",
                b"prior-executor-policy".as_slice(),
            ),
            (
                "etc/sy/spark-recipes.d/ornith-vllm.toml",
                b"prior-ornith-recipe".as_slice(),
            ),
            (
                "etc/sy/spark-recipes.d/qwen3-embedding.toml",
                b"prior-embedding-recipe".as_slice(),
            ),
        ] {
            assert_eq!(std::fs::read(root.path().join(relative)).unwrap(), prior);
        }
    }

    #[test]
    fn spark_agent_systemd_unit_verifies_and_pins_hardening() {
        let unit = include_str!("../../configs/systemd/system/sy-spark-agent.service");
        let profile = include_str!("../../configs/apparmor.d/sy-spark-agent");
        for directive in [
            "Type=notify",
            "User=sy-spark",
            "Group=sy-spark",
            "CapabilityBoundingSet=\n",
            "AmbientCapabilities=\n",
            "NoNewPrivileges=yes",
            "PrivateDevices=yes",
            "PrivateIPC=yes",
            "PrivateTmp=yes",
            "ProtectSystem=strict",
            "DevicePolicy=closed",
            "ReadWritePaths=/var/lib/sy-spark",
            "LimitNOFILE=4096",
            "TasksMax=128",
            "MemoryMax=256M",
            "OOMScoreAdjust=-100",
            "LoadCredential=bootstrap-token:",
            "WatchdogSec=30s",
            "Restart=on-failure",
            "RestartMaxDelaySec=30s",
            "AppArmorProfile=sy-spark-agent",
        ] {
            assert!(
                unit.contains(directive),
                "missing hardening directive: {directive}"
            );
        }
        if unit.contains("Type=notify") && unit.contains("WatchdogSec=") {
            for permission in [
                "network unix dgram,",
                "/run/systemd/notify w,",
                "/proc/*/cgroup r,",
                "/opt/sy-spark/hf-http-fallback/*/venv/pyvenv.cfg r,",
                "/opt/sy-spark/hf-http-fallback/*/venv/lib/** mr,",
                "/tmp/** rwkl,",
                "/dev/shm/sem.* rwkl,",
            ] {
                assert!(
                    profile.contains(permission),
                    "missing supervision permission: {permission}"
                );
            }
        }
        for denial in [
            "deny /var/run/docker.sock rw,",
            "deny /run/docker.sock rw,",
            "deny /home/** rwklx,",
        ] {
            assert!(profile.contains(denial), "missing confinement: {denial}");
        }
        let Some(systemd_analyze) = which::which("systemd-analyze").ok() else {
            return;
        };
        let output = std::process::Command::new(systemd_analyze)
            .args(["verify", "configs/systemd/system/sy-spark-agent.service"])
            .output()
            .unwrap();
        let hard_errors: Vec<_> = String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|line| {
                !line.contains("/opt/sy-spark/current/sy") || !line.contains("executable")
            })
            .filter(|line| !line.contains("SO_PASSRIGHTS") && !line.contains("SO_PASSCRED"))
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect();
        assert!(
            hard_errors.is_empty(),
            "systemd-analyze verify: {}",
            hard_errors.join("\n")
        );
    }

    #[test]
    fn spark_executor_unit_is_unix_only_group_reachable_and_hardened() {
        let unit = include_str!("../../configs/systemd/system/sy-spark-executor.service");
        let agent_unit = include_str!("../../configs/systemd/system/sy-spark-agent.service");
        let profile = include_str!("../../configs/apparmor.d/sy-spark-executor");
        for directive in [
            "Type=notify",
            "User=root",
            "Group=sy-spark",
            "After=docker.service",
            "Before=sy-spark-agent.service",
            "RuntimeDirectory=sy-spark",
            "RuntimeDirectoryMode=0750",
            "UMask=0007",
            "CapabilityBoundingSet=\n",
            "AmbientCapabilities=\n",
            "NoNewPrivileges=yes",
            "PrivateNetwork=yes",
            "ProtectControlGroups=no",
            "ProtectHome=yes",
            "ProtectProc=default",
            "ProtectSystem=strict",
            "ProcSubset=all",
            "RestrictAddressFamilies=AF_UNIX",
            "ReadOnlyPaths=/etc/sy/spark-executor.toml /etc/sy/spark-agent.toml /etc/sy/spark-recipes.d",
            "ReadWritePaths=/run/sy-spark /var/run/docker.sock /var/lib/sy-spark/executor /var/lib/sy-spark/compile-cache /sys/fs/cgroup/system.slice",
            "MemoryMax=64M",
            "WatchdogSec=30s",
            "Restart=on-failure",
            "AppArmorProfile=sy-spark-executor",
        ] {
            assert!(unit.contains(directive), "missing executor hardening: {directive}");
        }
        assert!(!unit.contains("AF_INET"));
        assert!(agent_unit.contains("Wants=network-online.target sy-spark-executor.service"));
        for permission in [
            "network unix stream,",
            "/run/sy-spark/executor.sock rwk,",
            "/var/run/docker.sock rw,",
            "/sys/fs/cgroup/**/cpu.max r,",
            "/sys/fs/cgroup/system.slice/docker-*.scope/cgroup.kill w,",
            "/proc/pressure/memory r,",
            "/proc/*/cgroup r,",
            "/proc/*/stat r,",
            "deny network inet,",
            "deny /home/** rwklx,",
        ] {
            assert!(
                profile.contains(permission),
                "missing executor policy: {permission}"
            );
        }
        let memory_rule = format!(
            "/sys/fs/cgroup/system.slice/docker-{}.scope/memory.current r,",
            "[0-9a-f]".repeat(64)
        );
        assert!(profile.contains(&memory_rule));
        assert!(!profile.contains(&memory_rule.replace(" r,", " rw,")));
        let source = include_str!("executor.rs");
        assert!(source.contains("Permissions::from_mode(0o660)"));
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn installer_materializes_the_actual_numeric_service_uid() {
        let root = tempfile::tempdir().unwrap();
        for relative in ["etc/sy", "etc/systemd/system", "etc/apparmor.d"] {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }
        let mut report = super::InstallReport {
            changed: true,
            actions: Vec::new(),
            fsync_trace: Vec::new(),
            preceding_release: None,
            active_release: PathBuf::from("opt/sy-spark/releases/test"),
        };
        super::write_policy_assets(root.path(), "10.1.30.143", 4242, &mut report).unwrap();
        let config =
            std::fs::read_to_string(root.path().join("etc/sy/spark-executor.toml")).unwrap();
        assert!(config.contains("agent_uid = 4242"));
        assert!(!config.contains("agent_uid = 996"));
    }

    #[cfg(feature = "spark-agent")]
    #[test]
    fn installer_materializes_the_supervision_target() {
        let root = tempfile::tempdir().unwrap();
        for relative in ["etc/sy", "etc/systemd/system", "etc/apparmor.d"] {
            std::fs::create_dir_all(root.path().join(relative)).unwrap();
        }
        let mut report = super::InstallReport {
            changed: true,
            actions: Vec::new(),
            fsync_trace: Vec::new(),
            preceding_release: None,
            active_release: PathBuf::from("opt/sy-spark/releases/test"),
        };
        super::write_policy_assets(root.path(), "10.1.30.143", 4242, &mut report).unwrap();

        let target =
            std::fs::read_to_string(root.path().join("etc/systemd/system/sy-spark.target"))
                .unwrap();
        assert!(target.contains("Requires=sy-spark-executor.service sy-spark-agent.service"));
    }
}

#[cfg(test)]
pub(crate) mod tests_fixture {
    pub const INVENTORY: &str = r#"{
        "schema":"sy.spark.bootstrap.inventory/v1","probe_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "hostname":"spark","dgx_software_build":"7.5.0","os":{"id":"ubuntu","version_id":"24.04","pretty_name":"Ubuntu 24.04"},
        "architecture":"aarch64","kernel_release":"6.17.0-1022-nvidia","nvidia_driver_version":"580.159.03","cuda_runtime_version":"13.0",
        "firmware_identity":"GB10:1.0","gpu":{"name":"NVIDIA GB10","compute_capability":"12.1"},
        "docker":{"version":"29.2.1","active":true,"login_user_socket_access":false},"nvidia_container_toolkit_version":"1.19.0","systemd_version":"255",
        "lsm":{"kind":"apparmor","mode":"enforce"},"python":{"version":"3.12.3","venv_available":true},
        "memory":{"total_bytes":127775277056,"available_bytes":123480309760,"swap_total_bytes":17179869184},
        "storage":[{"mount_point":"/","source":"/dev/nvme0n1p2","filesystem":"ext4","total_bytes":1000000000000,"free_bytes":793000000000}],
        "lan_addresses":["10.1.30.143"],"existing_installation":{"present":false,"current_release":null,"state_schema":null}
    }"#;
}
