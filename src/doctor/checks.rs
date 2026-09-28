//! Concrete `Check` implementations for `sy doctor` (SPEC §4.6,
//! ROADMAP `arch-observability` Step 5).
//!
//! Each check is a small struct that implements [`super::Check`]. The
//! [`default_checks`] builder returns them in SPEC-mandated order so
//! the JSON output is byte-stable across runs on the same host. All
//! probes are read-only and fail-soft: a host missing a probe surface
//! (no `/sys/kernel/security/lsm`, no `coredumpctl`, …) yields
//! [`Status::Skip`] with a `message` rather than crashing the runner.
//!
//! The `landlock_version_parses_lsm` helper is split out from the
//! `Check` impl so the SPEC §6 risk-row-4 parsing logic can be
//! exercised by unit tests without a real `/sys` mount.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use sy_ipc::paths::for_endpoint;

use super::{Check, CheckResult, Status};

/// SPEC §4.6 "first batch" of checks. Order is stable; consumers
/// (operators, CI greppers) rely on it.
///
/// BUG-20260927-1500 appends `supervision.cgroup_memory_throttle`, the
/// per-unit cgroup view that would have named the four-day outage in
/// `BUG-20260927-0910` while it was still happening.
///
/// sy-mon ROADMAP Step 21 appends the dashboard-plumbing checks
/// (`mon.collect.running`, one `mon.metrics_socket.<plane>` per known
/// plane, `mon.history.writable`) so the daily `sy doctor` sweep
/// covers the popup + aggregator surface.
pub fn default_checks() -> Vec<Box<dyn Check>> {
    let mut checks: Vec<Box<dyn Check>> = vec![
        Box::new(NpuDevice),
        Box::new(VitisaiCachePresent),
        Box::new(ModelArtifacts),
        Box::new(QdrantReachable),
        Box::new(QdrantVersionMin),
        Box::new(IpcEndpoint::knowledge()),
        Box::new(IpcEndpoint::aiplane()),
        Box::new(IpcEndpoint::agt()),
        Box::new(IpcEndpoint::stack()),
        Box::new(UserUnitsPresent),
        Box::new(ActiveSandboxScopes),
        Box::new(LandlockVersion),
        Box::new(SystemdUserSession),
        Box::new(CoredumpRecentCount),
        Box::new(CgroupMemoryThrottle),
    ];
    checks.extend(crate::mon::doctor::mon_doctor_checks());
    checks
}

// -- aiplane.npu.device ----------------------------------------------------

const NPU_DEVICE_PATH: &str = "/dev/accel/accel0";

pub struct NpuDevice;
impl Check for NpuDevice {
    fn name(&self) -> &'static str {
        "aiplane.npu.device"
    }
    fn run(&self) -> CheckResult {
        let p = Path::new(NPU_DEVICE_PATH);
        if p.exists() {
            CheckResult {
                name: self.name(),
                status: Status::Pass,
                message: Some(format!("{NPU_DEVICE_PATH} present")),
                fix: None,
                details: None,
            }
        } else {
            CheckResult {
                name: self.name(),
                status: Status::Fail,
                message: Some(format!("{NPU_DEVICE_PATH} missing")),
                fix: Some("load the amdxdna kernel module and ensure firmware is installed".into()),
                details: None,
            }
        }
    }
}

// -- aiplane.vitisai.cache_present ----------------------------------------

pub struct VitisaiCachePresent;
impl Check for VitisaiCachePresent {
    fn name(&self) -> &'static str {
        "aiplane.vitisai.cache_present"
    }
    fn run(&self) -> CheckResult {
        let dirs = model_dirs();
        let partitions = compiled_partitions(&dirs);
        let model_dirs_present = dirs.iter().any(|d| d.is_dir());
        let name = self.name();
        let details = Some(json!({
            "partitions": partitions.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "model_dirs": dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
        }));
        if !partitions.is_empty() {
            return CheckResult {
                name,
                status: Status::Pass,
                message: Some(format!(
                    "{} compiled vitisai partition(s) cached",
                    partitions.len()
                )),
                fix: None,
                details,
            };
        }
        if model_dirs_present {
            return CheckResult {
                name,
                status: Status::Warn,
                message: Some("model dirs exist but hold no compiled partition; the first                                dispatch pays a full AIE codegen (minutes)"
                    .into()),
                fix: Some(
                    "source /opt/AMD/ryzenai/venv/bin/activate && python \
                     ~/sources/sy/scripts/prep_npu_workload.py --workload embed".into(),
                ),
                details,
            };
        }
        CheckResult {
            name,
            status: Status::Skip,
            message: Some("no workload model dirs yet (prep_npu_workload.py has never run)".into()),
            fix: None,
            details,
        }
    }
}

/// Where each workload keeps its compiled VitisAI partition: sibling
/// `compiled_<cache-key>/` directories next to the model, as written by
/// `prep_npu_workload.py` and by the worker's own first cold compile.
///
/// This check used to stat `~/.cache/sy/aiplane/compile`, a path that has
/// never existed, so `aiplane.vitisai.cache_present` answered `skip` on
/// every host — including ones whose partitions were perfectly healthy.
/// Same family of blind spot as BUG-20260927-0119: a probe that cannot
/// observe the thing it claims to observe.
fn model_dirs() -> Vec<PathBuf> {
    crate::aiplane::workloads::artifact_sets()
        .iter()
        .filter_map(|set| set.model.parent().map(|p| p.to_path_buf()))
        .collect()
}

/// Every `compiled_*` directory holding at least one `.rai` artifact.
fn compiled_partitions(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let named_compiled = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("compiled_"));
            if !path.is_dir() || !named_compiled {
                continue;
            }
            let has_rai = fs::read_dir(&path).ok().is_some_and(|inner| {
                inner
                    .filter_map(|e| e.ok())
                    .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("rai"))
            });
            if has_rai {
                out.push(path);
            }
        }
    }
    out
}

// -- aiplane.model_artifacts ----------------------------------------------

/// Verdict for one workload's on-disk artifacts.
#[derive(Debug, PartialEq, Eq)]
pub enum ArtifactVerdict {
    /// NPU graph + tokenizer present.
    Ready,
    /// NPU graph gone (or dangling) but the FP32 sibling survived: the
    /// workload still loads on the CPU EP. Slow, but serving.
    CpuOnly,
    /// Nothing loadable. Carries the paths that were checked.
    Absent(Vec<String>),
}

/// Classify one artifact set from on-disk state. Paths are injected so the
/// Fail/Warn split is unit-testable without touching a real `~/.cache`.
///
/// `Absent` carries self-describing clauses ("tokenizer.json missing at …")
/// rather than bare paths, because this string is the operator's only
/// written diagnosis when the plane cannot embed.
pub(crate) fn classify_artifact(
    model: &Path,
    fallback: Option<&Path>,
    tokenizer: &Path,
) -> ArtifactVerdict {
    // No tokenizer means no encoding at all, regardless of which graph
    // survived — report it as the blocker.
    if !tokenizer.is_file() {
        return ArtifactVerdict::Absent(vec![format!(
            "tokenizer.json missing at {}",
            describe_missing(tokenizer)
        )]);
    }
    if model.is_file() {
        return ArtifactVerdict::Ready;
    }
    if fallback.is_some_and(|fb| fb.is_file()) {
        return ArtifactVerdict::CpuOnly;
    }
    let mut gaps = vec![format!("no model graph at {}", describe_missing(model))];
    if let Some(fb) = fallback {
        gaps.push(format!("no CPU-fallback graph at {}", describe_missing(fb)));
    }
    ArtifactVerdict::Absent(gaps)
}

/// Render a path that failed the `is_file` test, calling out a dangling
/// symlink explicitly — `Path::is_file()` follows symlinks, so the wiped
/// `~/.cache/sy/npu-embed` targets behind `~/.cache/sy/aiplane/<stem>/*.onnx`
/// read as "not found" and sent the operator hunting for a file that
/// *appeared* in `ls -l` (BUG-20260927-0119).
fn describe_missing(path: &Path) -> String {
    if path.symlink_metadata().is_ok() && !path.exists() {
        let target = std::fs::read_link(path)
            .map(|t| t.display().to_string())
            .unwrap_or_else(|_| "?".into());
        return format!("{} (dangling symlink -> {target})", path.display());
    }
    path.display().to_string()
}

/// Pre-flight the ONNX + tokenizer files every start-up workload needs.
///
/// Without this, a wiped model cache produced `fail: 0` in
/// `sy doctor --json` while the whole memory plane was down for four
/// days: the loader's message only ever reached journald, and the daemon
/// died before it could serve the IPC socket the other doctor probes use.
pub struct ModelArtifacts;

impl Check for ModelArtifacts {
    fn name(&self) -> &'static str {
        "aiplane.model_artifacts"
    }

    fn run(&self) -> CheckResult {
        let sets = crate::aiplane::workloads::artifact_sets();
        let mut hard: Vec<String> = Vec::new();
        let mut soft: Vec<String> = Vec::new();
        let mut preps: Vec<String> = Vec::new();
        let mut ready = 0usize;
        for set in &sets {
            match classify_artifact(&set.model, set.fallback_model.as_deref(), &set.tokenizer) {
                ArtifactVerdict::Ready => ready += 1,
                ArtifactVerdict::CpuOnly => {
                    soft.push(format!(
                        "{}: bf16 export missing at {} (CPU EP fallback available)",
                        set.kind,
                        set.model.display()
                    ));
                    preps.push(set.prep.clone());
                }
                ArtifactVerdict::Absent(paths) => {
                    soft_or_hard(
                        set.indexing_critical,
                        &mut hard,
                        &mut soft,
                        format!("{}: not loadable ({})", set.kind, paths.join("; ")),
                    );
                    preps.push(set.prep.clone());
                }
            }
        }
        verdict_report(hard, soft, preps, ready, sets.len())
    }
}

/// Route one gap message by criticality: a non-critical set (rerank) must
/// not turn the plane red, mirroring `knowledge::daemon::indexing_blocked`.
fn soft_or_hard(critical: bool, hard: &mut Vec<String>, soft: &mut Vec<String>, message: String) {
    if critical {
        hard.push(message);
    } else {
        soft.push(message);
    }
}

/// Fail only on critical gaps; warn on everything else; pass when every set
/// is complete. Split out of `run` so the mapping is testable.
fn verdict_report(
    hard: Vec<String>,
    soft: Vec<String>,
    preps: Vec<String>,
    ready: usize,
    total: usize,
) -> CheckResult {
    let name = "aiplane.model_artifacts";
    let details = Some(json!({ "ready": ready, "total": total }));
    let fix = (!preps.is_empty()).then(|| {
        format!(
            "source /opt/AMD/ryzenai/venv/bin/activate && {} (then `systemctl --user restart sy-knowledge.service`)",
            preps.join(" && ")
        )
    });
    if !hard.is_empty() {
        return CheckResult {
            name,
            status: Status::Fail,
            message: Some(hard.join("; ")),
            fix,
            details,
        };
    }
    if !soft.is_empty() {
        return CheckResult {
            name,
            status: Status::Warn,
            message: Some(soft.join("; ")),
            fix,
            details,
        };
    }
    CheckResult {
        name,
        status: Status::Pass,
        message: Some(format!("{ready}/{total} workload artifact sets present")),
        fix: None,
        details,
    }
}

// -- knowledge.qdrant_reachable -------------------------------------------

const QDRANT_HOST: &str = "127.0.0.1";
const QDRANT_PORT: u16 = 6333;
const TCP_CONNECT_TIMEOUT_MS: u64 = 500;

pub struct QdrantReachable;
impl Check for QdrantReachable {
    fn name(&self) -> &'static str {
        "knowledge.qdrant_reachable"
    }
    fn run(&self) -> CheckResult {
        let addr = format!("{QDRANT_HOST}:{QDRANT_PORT}");
        let socket_addrs = match addr.parse() {
            Ok(a) => a,
            Err(e) => {
                return CheckResult {
                    name: self.name(),
                    status: Status::Fail,
                    message: Some(format!("address parse: {e}")),
                    fix: None,
                    details: None,
                };
            }
        };
        match std::net::TcpStream::connect_timeout(
            &socket_addrs,
            Duration::from_millis(TCP_CONNECT_TIMEOUT_MS),
        ) {
            Ok(_) => CheckResult {
                name: self.name(),
                status: Status::Pass,
                message: Some(format!("tcp reachable on {addr}")),
                fix: None,
                details: Some(json!({ "note": "tcp reachable", "addr": addr })),
            },
            Err(e) => CheckResult {
                name: self.name(),
                status: Status::Fail,
                message: Some(format!("connect {addr}: {e}")),
                fix: Some("start qdrant: `systemctl --user start sy-qdrant.service`".into()),
                details: None,
            },
        }
    }
}

// -- knowledge.qdrant.version_min_1_16 ------------------------------------

/// Probe the live qdrant version against the minimum the hybrid Universal
/// Query needs. qdrant < 1.16 silently ignores the configurable RRF `k`
/// (`query.rrf.k = 60`), so hybrid search regresses with no error
/// (knowledge-retrieval-iter1 cross-cutting DoD). `GET /` returns
/// `{"version":"1.x.y",...}`; we classify it pass/fail and treat an
/// unreachable qdrant as `warn` (the daemon may simply be down — hard
/// reachability is `knowledge.qdrant_reachable`'s job).
pub struct QdrantVersionMin;

const QDRANT_ROOT_TIMEOUT_MS: u64 = 500;

impl QdrantVersionMin {
    /// Fetch the qdrant root `GET /` body, or `None` when unreachable.
    fn fetch_root() -> Option<String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(QDRANT_ROOT_TIMEOUT_MS))
            .build()
            .ok()?;
        let resp = client
            .get(format!("http://{QDRANT_HOST}:{QDRANT_PORT}/"))
            .send()
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.text().ok()
    }

    /// Classify a (possibly absent) root body into a `CheckResult`. Pure
    /// over its input so the pass/fail/warn mapping is unit-testable.
    fn classify(&self, root_body: Option<String>) -> CheckResult {
        use crate::knowledge::qdrant::{meets_min_version, parse_version, MIN_HYBRID_VERSION};
        let (min_major, min_minor) = MIN_HYBRID_VERSION;
        match root_body.as_deref().and_then(parse_version) {
            Some(v) if meets_min_version(v, MIN_HYBRID_VERSION) => CheckResult {
                name: self.name(),
                status: Status::Pass,
                message: Some(format!(
                    "qdrant {}.{} ≥ {min_major}.{min_minor} (hybrid RRF k ok)",
                    v.0, v.1
                )),
                fix: None,
                details: Some(json!({ "major": v.0, "minor": v.1 })),
            },
            Some(v) => CheckResult {
                name: self.name(),
                status: Status::Fail,
                message: Some(format!(
                    "qdrant {}.{} < {min_major}.{min_minor}; hybrid RRF k silently ignored",
                    v.0, v.1
                )),
                fix: Some("run `sy apply` to upgrade qdrant".into()),
                details: Some(json!({ "major": v.0, "minor": v.1 })),
            },
            None => CheckResult {
                name: self.name(),
                status: Status::Warn,
                message: Some("qdrant not running or version unreadable".into()),
                fix: Some("start qdrant: `systemctl --user start sy-qdrant.service`".into()),
                details: None,
            },
        }
    }
}

impl Check for QdrantVersionMin {
    fn name(&self) -> &'static str {
        "knowledge.qdrant.version_min_1_16"
    }
    fn run(&self) -> CheckResult {
        self.classify(Self::fetch_root())
    }
}

// -- ipc.knowledge_sock / ipc.aiplane_sock --------------------------------

const IPC_SYSTEM_HEALTH_TIMEOUT_MS: u64 = 1_500;

pub struct IpcEndpoint {
    name: &'static str,
    endpoint: &'static str,
    /// How to learn whether the owning unit is enabled. Injectable so the
    /// Warn/Skip split in [`IpcEndpoint::classify_missing`] is testable off
    /// a live user manager — asserting on the real `systemctl` here would
    /// make the outcome depend on this machine's rice (AGENTS.md: a flaky
    /// test is a bug).
    unit_state: fn(&str) -> Option<bool>,
}

impl IpcEndpoint {
    pub fn knowledge() -> Self {
        Self {
            name: "ipc.knowledge_sock",
            endpoint: "knowledge",
            unit_state: user_unit_enabled,
        }
    }
    pub fn aiplane() -> Self {
        Self {
            name: "ipc.aiplane_sock",
            endpoint: "aiplane",
            unit_state: user_unit_enabled,
        }
    }
    pub fn agt() -> Self {
        Self {
            name: "ipc.agt_sock",
            endpoint: "agt",
            unit_state: user_unit_enabled,
        }
    }
    pub fn stack() -> Self {
        Self {
            name: "ipc.stack_sock",
            endpoint: "stack",
            unit_state: user_unit_enabled,
        }
    }

    /// The user unit that owns this socket, or `None` when no unit does.
    ///
    /// Not `format!("sy-{endpoint}.service")`: the aiplane facade is served
    /// by the knowledge daemon on `sy-knowledge.sock`
    /// (`aiplane::ipc::socket_path`), and the agent daemon's unit is
    /// `sy-agentd.service`. The interpolation used to advise
    /// `systemctl --user start sy-aiplane.service` — a unit that has never
    /// existed — on a host whose knowledge plane was down.
    /// Swap the unit-state probe (tests).
    #[cfg(test)]
    fn with_unit_state(mut self, probe: fn(&str) -> Option<bool>) -> Self {
        self.unit_state = probe;
        self
    }

    pub fn unit(&self) -> Option<&'static str> {
        match self.endpoint {
            "knowledge" | "aiplane" => Some("sy-knowledge.service"),
            "agt" => Some("sy-agentd.service"),
            "stack" => Some("sy-stack-bar.service"),
            _ => None,
        }
    }

    fn fix_for_unit(&self) -> Option<String> {
        self.unit().map(|unit| {
            format!(
                "start it: `systemctl --user restart {unit}`; diagnose: `journalctl --user -u {unit} -n 50`"
            )
        })
    }

    /// Socket absent. This used to be an unconditional `Skip`, which is how
    /// a four-day outage on an *enabled* unit scored `fail: 0`. Enabled +
    /// no socket is a fault; disabled / unknown stays a skip.
    pub(crate) fn classify_missing(&self, sock: &Path, unit_enabled: Option<bool>) -> CheckResult {
        let (status, note) = match unit_enabled {
            Some(true) => (
                Status::Warn,
                "unit is enabled but the socket is absent — the daemon is failing to start",
            ),
            Some(false) => (Status::Skip, "unit is disabled on this host"),
            None => (Status::Skip, "unit state unknown (no user bus?)"),
        };
        CheckResult {
            name: self.name,
            status,
            message: Some(format!("{} not present — {note}", sock.display())),
            fix: self.fix_for_unit(),
            details: Some(json!({
                "socket": sock.display().to_string(),
                "unit": self.unit(),
                "unit_enabled": unit_enabled,
            })),
        }
    }
}

impl Check for IpcEndpoint {
    fn name(&self) -> &'static str {
        self.name
    }
    fn run(&self) -> CheckResult {
        let sock = match for_endpoint(self.endpoint) {
            Some(s) => s,
            None => {
                return CheckResult {
                    name: self.name,
                    status: Status::Fail,
                    message: Some(format!("unknown ipc endpoint {:?}", self.endpoint)),
                    fix: None,
                    details: None,
                };
            }
        };
        if !sock.exists() {
            return self.classify_missing(sock.as_path(), self.unit().and_then(self.unit_state));
        }
        match probe_system_health(&sock) {
            Ok(state) => CheckResult {
                name: self.name,
                status: if state == "ready" {
                    Status::Pass
                } else {
                    Status::Warn
                },
                message: Some(format!("state={state}")),
                fix: None,
                details: Some(json!({
                    "socket": sock.display().to_string(),
                    "state": state,
                })),
            },
            Err(e) => CheckResult {
                name: self.name,
                status: Status::Fail,
                message: Some(format!("{}: {e}", sock.display())),
                fix: self.fix_for_unit(),
                details: None,
            },
        }
    }
}

/// `systemctl --user is-enabled <unit>` as a tri-state: `Some(true)` when
/// enabled, `Some(false)` when disabled / masked / not installed, `None`
/// when systemctl cannot answer at all (CI containers, no user bus).
fn user_unit_enabled(unit: &str) -> Option<bool> {
    Command::new("systemctl")
        .args(["--user", "is-enabled", unit])
        .output()
        .ok()
        .map(|out| out.status.success())
}

/// Synchronous `system.health` round-trip. The doctor runner is
/// synchronous and we don't want each invocation to spin up a tokio
/// runtime per check, so we frame the request ourselves over a blocking
/// `UnixStream` using the same length-delimited shape the async codec
/// emits. This matches SPEC §4.2 framing and `sy_ipc::codec`.
///
/// `pub(crate)` so the sy-mon doctor checks (`src/mon/doctor.rs`) can
/// reuse the same probe for `$XDG_RUNTIME_DIR/sy/mon.sock` without
/// dragging in a tokio runtime per check (sy-mon ROADMAP Step 21).
pub(crate) fn probe_system_health(sock: &Path) -> Result<String, String> {
    let mut stream = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(IPC_SYSTEM_HEALTH_TIMEOUT_MS)))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_millis(IPC_SYSTEM_HEALTH_TIMEOUT_MS)))
        .map_err(|e| format!("set_write_timeout: {e}"))?;
    let body = json!({
        "schema_version": sy_ipc::SCHEMA_VERSION,
        "request_id": ulid::Ulid::new().to_string(),
        "method": "system.health",
        "params": {},
        "priority": "Interactive",
        "deadline_ms": IPC_SYSTEM_HEALTH_TIMEOUT_MS,
    });
    let bytes = serde_json::to_vec(&body).map_err(|e| format!("encode: {e}"))?;
    let len = u32::try_from(bytes.len()).map_err(|_| "request too large".to_string())?;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|e| format!("write len: {e}"))?;
    stream
        .write_all(&bytes)
        .map_err(|e| format!("write body: {e}"))?;
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("read len: {e}"))?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp = vec![0u8; resp_len];
    stream
        .read_exact(&mut resp)
        .map_err(|e| format!("read body: {e}"))?;
    let v: serde_json::Value = serde_json::from_slice(&resp).map_err(|e| format!("decode: {e}"))?;
    let state = v
        .get("result")
        .and_then(|r| r.get("state"))
        .and_then(|s| s.as_str())
        .ok_or_else(|| "missing result.state".to_string())?;
    Ok(state.to_string())
}

// -- supervision.user_units_present ---------------------------------------

const USER_UNIT_TARGET: &str = "sy.target";

pub struct UserUnitsPresent;
impl Check for UserUnitsPresent {
    fn name(&self) -> &'static str {
        "supervision.user_units_present"
    }
    fn run(&self) -> CheckResult {
        let dir = match user_systemd_dir() {
            Some(d) => d,
            None => {
                return CheckResult {
                    name: self.name(),
                    status: Status::Skip,
                    message: Some("HOME unset; cannot resolve user systemd dir".into()),
                    fix: None,
                    details: None,
                };
            }
        };
        if !dir.is_dir() {
            return CheckResult {
                name: self.name(),
                status: Status::Skip,
                message: Some(format!("{} does not exist", dir.display())),
                fix: None,
                details: None,
            };
        }
        let target = dir.join(USER_UNIT_TARGET);
        if target.exists() {
            CheckResult {
                name: self.name(),
                status: Status::Pass,
                message: Some(format!("{} present", target.display())),
                fix: None,
                details: None,
            }
        } else {
            CheckResult {
                name: self.name(),
                status: Status::Fail,
                message: Some(format!("{} missing", target.display())),
                fix: Some("run `sy apply` to install the user systemd units".into()),
                details: None,
            }
        }
    }
}

fn user_systemd_dir() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/systemd/user"))
}

// -- agent.sandbox.active_scopes ------------------------------------------

/// Reports the count of active user-manager `.scope` units — a proxy
/// for "agent sandbox scopes currently live" (arch-agent-sandbox Step
/// 4). `sy` doesn't yet stamp a unique prefix on its transient scopes
/// (`systemd-run --user --scope` defaults to `run-<pid>.scope`), so we
/// report the total count as informational rather than filtering by
/// prefix. A future refinement (when `sy agentd` names its scopes
/// `sy-sandbox-<ulid>.scope`) can tighten the filter.
///
/// Status mapping:
/// - `Skip`   — `systemctl` not on PATH (no user manager surface).
/// - `Pass`   — `systemctl` succeeded; `details.count` carries N.
/// - `Warn`   — `systemctl` ran but exited non-zero (transient
///   user-manager hiccup); operators see the stderr in `message`.
pub struct ActiveSandboxScopes;

const SYSTEMCTL: &str = "systemctl";

impl ActiveSandboxScopes {
    /// Test seam: a `Some("")` PATH lets the unit test exercise the
    /// "systemctl missing" branch without mutating the process env.
    /// `None` uses the inherited PATH.
    fn run_with_path(&self, path_override: Option<&str>) -> CheckResult {
        let mut cmd = Command::new(SYSTEMCTL);
        cmd.args(["--user", "list-units", "--type=scope", "--no-legend"]);
        if let Some(p) = path_override {
            cmd.env("PATH", p);
        }
        match cmd.output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CheckResult {
                name: self.name(),
                status: Status::Skip,
                message: Some("systemctl not on PATH".into()),
                fix: None,
                details: None,
            },
            Err(e) => CheckResult {
                name: self.name(),
                status: Status::Skip,
                message: Some(format!("systemctl failed: {e}")),
                fix: None,
                details: None,
            },
            Ok(o) if !o.status.success() => CheckResult {
                name: self.name(),
                status: Status::Warn,
                message: Some(format!(
                    "systemctl --user list-units exited {}",
                    o.status.code().unwrap_or(-1)
                )),
                fix: Some("check `systemctl --user status` for a healthy user manager".into()),
                details: None,
            },
            Ok(o) => {
                let count = parse_scope_count(&o.stdout);
                CheckResult {
                    name: self.name(),
                    status: Status::Pass,
                    message: Some(format!("{count} active scope unit(s)")),
                    fix: None,
                    details: Some(json!({ "count": count })),
                }
            }
        }
    }
}

impl Check for ActiveSandboxScopes {
    fn name(&self) -> &'static str {
        "agent.sandbox.active_scopes"
    }
    fn run(&self) -> CheckResult {
        self.run_with_path(None)
    }
}

/// Count non-blank lines from `systemctl --user list-units --type=scope
/// --no-legend` stdout. `--no-legend` strips the header and trailing
/// summary, leaving one unit per line; we treat any non-whitespace line
/// as one scope. Malformed UTF-8 degrades to zero (best-effort probe).
fn parse_scope_count(stdout: &[u8]) -> usize {
    match std::str::from_utf8(stdout) {
        Ok(s) => s.lines().filter(|l| !l.trim().is_empty()).count(),
        Err(_) => 0,
    }
}

// -- kernel.landlock_version ----------------------------------------------

const LSM_PATH: &str = "/sys/kernel/security/lsm";
const LANDLOCK_TOKEN: &str = "landlock";

pub struct LandlockVersion;
impl Check for LandlockVersion {
    fn name(&self) -> &'static str {
        "kernel.landlock_version"
    }
    fn run(&self) -> CheckResult {
        match fs::read_to_string(LSM_PATH) {
            Ok(s) => match landlock_token(&s) {
                Some(tok) => CheckResult {
                    name: self.name(),
                    status: Status::Pass,
                    message: Some(format!("{LSM_PATH} reports landlock present")),
                    fix: None,
                    details: Some(json!({ "lsm": s.trim(), "token": tok })),
                },
                None => CheckResult {
                    name: self.name(),
                    status: Status::Warn,
                    message: Some(format!("{LSM_PATH} has no `landlock` entry")),
                    fix: Some("kernel ≥ 5.13 with CONFIG_SECURITY_LANDLOCK=y required".into()),
                    details: Some(json!({ "lsm": s.trim() })),
                },
            },
            Err(e) => CheckResult {
                name: self.name(),
                status: Status::Skip,
                message: Some(format!("{LSM_PATH}: {e}")),
                fix: None,
                details: None,
            },
        }
    }
}

/// Extract the `landlock` token from the comma-separated content of
/// `/sys/kernel/security/lsm` (SPEC §6 risk row 4). The kernel does
/// not expose the ABI level here — only presence — so we report the
/// token and let the `LandlockVersion` check map presence/absence to
/// pass/warn.
pub fn landlock_token(lsm: &str) -> Option<&'static str> {
    if lsm.trim().split(',').any(|t| t.trim() == LANDLOCK_TOKEN) {
        Some(LANDLOCK_TOKEN)
    } else {
        None
    }
}

// -- kernel.systemd_user_session ------------------------------------------

pub struct SystemdUserSession;
impl Check for SystemdUserSession {
    fn name(&self) -> &'static str {
        "kernel.systemd_user_session"
    }
    fn run(&self) -> CheckResult {
        match env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() && Path::new(&dir).is_dir() => CheckResult {
                name: self.name(),
                status: Status::Pass,
                message: Some(format!(
                    "XDG_RUNTIME_DIR={} present",
                    Path::new(&dir).display()
                )),
                fix: None,
                details: None,
            },
            _ => CheckResult {
                name: self.name(),
                status: Status::Fail,
                message: Some("XDG_RUNTIME_DIR unset or not a directory".into()),
                fix: Some("ensure `loginctl enable-linger $USER` and a fresh login".into()),
                details: None,
            },
        }
    }
}

// -- coredump.recent_count -------------------------------------------------

const COREDUMPCTL_TIMEOUT_S: u64 = 5;
const COREDUMPCTL_SINCE: &str = "-1day";

pub struct CoredumpRecentCount;
impl Check for CoredumpRecentCount {
    fn name(&self) -> &'static str {
        "coredump.recent_count"
    }
    fn run(&self) -> CheckResult {
        let out = Command::new("coredumpctl")
            .args(["list", "--json=pretty", "--since", COREDUMPCTL_SINCE])
            .output();
        match out {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CheckResult {
                name: self.name(),
                status: Status::Skip,
                message: Some("coredumpctl not on PATH".into()),
                fix: None,
                details: None,
            },
            Err(e) => CheckResult {
                name: self.name(),
                status: Status::Skip,
                message: Some(format!("coredumpctl failed: {e}")),
                fix: None,
                details: None,
            },
            Ok(o) if !o.status.success() => {
                // coredumpctl exits non-zero when there are no cores;
                // surface that as a clean pass with count=0 per the
                // SPEC §4.6 "N cores in last 24 h" intent.
                CheckResult {
                    name: self.name(),
                    status: Status::Pass,
                    message: Some("no cores in last 24h".into()),
                    fix: None,
                    details: Some(json!({ "count": 0, "since": COREDUMPCTL_SINCE })),
                }
            }
            Ok(o) => {
                let count = parse_coredumpctl_count(&o.stdout);
                let status = if count > 0 {
                    Status::Warn
                } else {
                    Status::Pass
                };
                CheckResult {
                    name: self.name(),
                    status,
                    message: Some(format!("{count} cores in last 24h")),
                    fix: if count > 0 {
                        Some("run `sy crash list` to investigate".into())
                    } else {
                        None
                    },
                    details: Some(json!({ "count": count, "since": COREDUMPCTL_SINCE })),
                }
            }
        }
    }
}

/// Parse the `coredumpctl list --json=pretty` array length.
/// `--json=pretty` returns a JSON array; older `coredumpctl` versions
/// emit nothing or a non-array — those degrade to `0` rather than
/// crashing the runner (this check is best-effort).
fn parse_coredumpctl_count(stdout: &[u8]) -> usize {
    let _ = COREDUMPCTL_TIMEOUT_S;
    match serde_json::from_slice::<serde_json::Value>(stdout) {
        Ok(serde_json::Value::Array(a)) => a.len(),
        _ => 0,
    }
}

// -- supervision.cgroup_memory_throttle ------------------------------------

/// Fraction of `MemoryHigh` at which a unit's observed peak becomes worth
/// a WARN. BUG-20260927-0910: a cold whisper compile peaked at 19.5 GB
/// inside the knowledge unit's 12 GiB cap, so the cap was the outage, not
/// a safety net — 90 % headroom consumed is the same story one step early.
const CGROUP_PEAK_HEADROOM_WARN: f64 = 0.90;

/// PSI `full avg10` (percent of the window the cgroup was fully stalled)
/// at which a unit gets a WARN. Host-wide `sy mon` already samples PSI;
/// this is the per-unit view that a `sy doctor` run needs to explain a
/// slow plane.
const CGROUP_PRESSURE_FULL_WARN: f64 = 10.0;

/// The four `memory.events` counters that turn a cap into an incident.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MemoryEvents {
    high: u64,
    max: u64,
    oom: u64,
    oom_kill: u64,
}

impl MemoryEvents {
    /// Any throttling or OOM activity recorded since the unit started.
    fn is_quiet(&self) -> bool {
        self.high == 0 && self.max == 0 && self.oom == 0 && self.oom_kill == 0
    }
}

pub struct CgroupMemoryThrottle;
impl Check for CgroupMemoryThrottle {
    fn name(&self) -> &'static str {
        "supervision.cgroup_memory_throttle"
    }
    fn run(&self) -> CheckResult {
        let skip = |why: String| CheckResult {
            name: self.name(),
            status: Status::Skip,
            message: Some(why),
            fix: None,
            details: None,
        };
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR").unwrap_or_default();
        let Some(slice) = user_cgroup_slice(&runtime_dir) else {
            return skip(format!(
                "cannot derive the uid from XDG_RUNTIME_DIR={}",
                runtime_dir.to_string_lossy()
            ));
        };
        let Ok(entries) = fs::read_dir(user_systemd_dir().unwrap_or_default()) else {
            return skip("no user systemd dir to enumerate units from".into());
        };
        let mut units: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("sy-") && n.ends_with(".service"))
            .collect();
        units.sort();

        let mut inspected = 0usize;
        let mut worst = Status::Pass;
        let mut offenders: Vec<String> = Vec::new();
        let mut per_unit = serde_json::Map::new();
        for unit in units {
            let Some(cgroup) = unit_cgroup_dir(&slice, &unit) else {
                continue;
            };
            // A unit that is not currently running has no cgroup directory;
            // that is not a memory finding, so it is simply not inspected.
            if !cgroup.join("memory.events").is_file() {
                continue;
            }
            inspected += 1;
            let events = parse_memory_events(&read_or_empty(&cgroup.join("memory.events")));
            let full_avg10 =
                parse_pressure_full_avg10(&read_or_empty(&cgroup.join("memory.pressure")));
            let high = parse_cgroup_limit(&read_or_empty(&cgroup.join("memory.high")));
            let peak = parse_cgroup_limit(&read_or_empty(&cgroup.join("memory.peak")));
            let current = parse_cgroup_limit(&read_or_empty(&cgroup.join("memory.current")));
            let (status, reason) = throttle_verdict(events, full_avg10, high, peak);
            if status_rank(status) > status_rank(worst) {
                worst = status;
            }
            if status != Status::Pass {
                offenders.push(format!("{unit}: {reason}"));
            }
            per_unit.insert(
                unit.clone(),
                json!({
                    "status": status,
                    "reason": reason,
                    "memory_events": {
                        "high": events.high,
                        "max": events.max,
                        "oom": events.oom,
                        "oom_kill": events.oom_kill,
                    },
                    "psi_full_avg10": full_avg10,
                    "memory_high_bytes": high,
                    "memory_peak_bytes": peak,
                    "memory_current_bytes": current,
                }),
            );
        }

        if inspected == 0 {
            return skip("no running sy unit exposes a memory cgroup".into());
        }
        let message = if offenders.is_empty() {
            format!("{inspected} supervised unit(s) free of memory throttling")
        } else {
            offenders.join("; ")
        };
        CheckResult {
            name: self.name(),
            status: worst,
            fix: (worst != Status::Pass).then(|| {
                "raise MemoryHigh=/MemoryMax= for the unit in configs/systemd/user and run                  `sy apply` (rationale + the measured peak belong in the unit comment; see                  specs/bugs/BUG-20260927-0910.md)"
                    .to_string()
            }),
            details: Some(json!({ "inspected": inspected, "units": per_unit })),
            message: Some(message),
        }
    }
}

/// `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service` from
/// `$XDG_RUNTIME_DIR`, whose systemd contract is `/run/user/<uid>`.
/// `None` when the variable is unset or its last component is not numeric.
fn user_cgroup_slice(runtime_dir: &std::ffi::OsStr) -> Option<String> {
    let uid = runtime_dir
        .to_str()?
        .trim_end_matches('/')
        .rsplit('/')
        .next()?;
    if uid.is_empty() || !uid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ))
}

/// The cgroup directory systemd gives a user unit under the delegated
/// `app.slice` (cgroup v2, unified hierarchy).
fn unit_cgroup_dir(slice: &str, unit: &str) -> Option<PathBuf> {
    Some(Path::new(slice).join("app.slice").join(unit))
}

/// Parse cgroup v2 `memory.events` (`key <count>` per line), keeping only
/// the four counters the verdict acts on. Unparseable lines are ignored.
fn parse_memory_events(text: &str) -> MemoryEvents {
    let mut ev = MemoryEvents::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(key), Some(val)) = (it.next(), it.next()) else {
            continue;
        };
        let Ok(n) = val.parse::<u64>() else { continue };
        match key {
            "high" => ev.high = n,
            "max" => ev.max = n,
            "oom" => ev.oom = n,
            "oom_kill" => ev.oom_kill = n,
            _ => {}
        }
    }
    ev
}

/// Parse the `full avg10=` field of a cgroup `*.pressure` file.
fn parse_pressure_full_avg10(text: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.trim_start().starts_with("full "))?
        .split_whitespace()
        .find(|f| f.starts_with("avg10="))?
        .trim_start_matches("avg10=")
        .parse()
        .ok()
}

/// Parse a cgroup limit file: a byte count, or `max`/absent → `None`.
fn parse_cgroup_limit(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

fn read_or_empty(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn status_rank(status: Status) -> u8 {
    match status {
        Status::Pass => 0,
        Status::Skip => 1,
        Status::Warn => 2,
        Status::Fail => 3,
    }
}

/// Judge one unit's memory cgroup. Counters are since-unit-start, which is
/// exactly what an operator wants: a restart clears them, a lingering cap
/// problem does not.
fn throttle_verdict(
    events: MemoryEvents,
    full_avg10: Option<f64>,
    high: Option<u64>,
    peak: Option<u64>,
) -> (Status, String) {
    if events.oom_kill > 0 {
        return (
            Status::Fail,
            format!(
                "{} process(es) OOM-killed in the unit cgroup",
                events.oom_kill
            ),
        );
    }
    if events.oom > 0 {
        return (
            Status::Fail,
            format!("{} OOM occurrence(s) in the unit cgroup", events.oom),
        );
    }
    if events.high > 0 {
        return (
            Status::Warn,
            format!(
                "MemoryHigh reached {} time(s) since unit start; workers parked by the kernel",
                events.high
            ),
        );
    }
    if events.max > 0 {
        return (
            Status::Warn,
            format!("MemoryMax reached {} time(s) since unit start", events.max),
        );
    }
    if full_avg10.is_some_and(|p| p >= CGROUP_PRESSURE_FULL_WARN) {
        return (
            Status::Warn,
            format!(
                "memory pressure full avg10 {:.1}% (>= {CGROUP_PRESSURE_FULL_WARN:.0}%)",
                full_avg10.unwrap_or_default()
            ),
        );
    }
    if let (Some(high), Some(peak)) = (high, peak) {
        if high > 0 && peak as f64 >= high as f64 * CGROUP_PEAK_HEADROOM_WARN {
            return (
                Status::Warn,
                format!(
                    "peak {} MiB is within {:.0}% of MemoryHigh {} MiB",
                    peak / 1024 / 1024,
                    CGROUP_PEAK_HEADROOM_WARN * 100.0,
                    high / 1024 / 1024
                ),
            );
        }
    }
    (
        Status::Pass,
        if events.is_quiet() {
            "no throttling since unit start".to_string()
        } else {
            "cgroup quiet".to_string()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // BUG-20260927-0119: the plane was dead for four days with
    // `summary.fail == 0`. These pin the two blind spots.

    #[test]
    fn cgroup_slice_derives_the_uid_from_xdg_runtime_dir() {
        let slice = user_cgroup_slice(std::ffi::OsStr::new("/run/user/1000")).expect("uid 1000");
        assert_eq!(
            slice,
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service"
        );
        let unit = unit_cgroup_dir(&slice, "sy-knowledge.service").expect("unit dir");
        assert_eq!(
            unit,
            PathBuf::from(
                "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/sy-knowledge.service"
            )
        );
        for bogus in ["", "/", "/run/user", "/run/user/abc"] {
            assert!(
                user_cgroup_slice(std::ffi::OsStr::new(bogus)).is_none(),
                "{bogus:?} must not resolve to a cgroup slice"
            );
        }
    }

    #[test]
    fn memory_events_and_pressure_parse_the_kernel_format() {
        let ev = parse_memory_events(
            "low 0\nhigh 347871\nmax 0\nom 0\nom_kill 0\nom_group_kill 0\nsock_throttled 0\n",
        );
        assert_eq!(
            ev,
            MemoryEvents {
                high: 347_871,
                max: 0,
                oom: 0,
                oom_kill: 0
            }
        );
        assert_eq!(
            parse_pressure_full_avg10(
                "some avg10=69.03 avg60=61.10 avg300=40.02 total=1\nfull avg10=69.03 avg60=61.10 avg300=40.02 total=1"
            ),
            Some(69.03)
        );
        assert_eq!(parse_pressure_full_avg10("some avg10=1.0 total=0"), None);
        assert_eq!(parse_cgroup_limit("max\n"), None);
        assert_eq!(parse_cgroup_limit("17179869184\n"), Some(17_179_869_184));
    }

    #[test]
    fn throttle_verdict_names_the_outage_shapes() {
        let quiet = MemoryEvents::default();
        // BUG-20260927-0910: the four-day outage, as the kernel saw it.
        assert_eq!(
            throttle_verdict(
                MemoryEvents {
                    high: 347_871,
                    max: 0,
                    oom: 0,
                    oom_kill: 0
                },
                Some(69.03),
                Some(12 * 1024 * 1024 * 1024),
                Some(19 * 1024 * 1024 * 1024)
            )
            .0,
            Status::Warn
        );
        assert_eq!(
            throttle_verdict(
                MemoryEvents {
                    high: 0,
                    max: 0,
                    oom: 0,
                    oom_kill: 3
                },
                None,
                None,
                None
            )
            .0,
            Status::Fail
        );
        // No counter yet, but the cap is all but consumed: still a WARN.
        let (status, reason) = throttle_verdict(
            quiet,
            Some(0.0),
            Some(16 * 1024 * 1024 * 1024),
            Some(15 * 1024 * 1024 * 1024),
        );
        assert_eq!(status, Status::Warn, "{reason}");
        assert!(reason.contains("MemoryHigh"), "{reason}");
        // Pressure without any counter crossing is a WARN too.
        assert_eq!(
            throttle_verdict(quiet, Some(12.5), Some(16 * 1024 * 1024 * 1024), Some(1024)).0,
            Status::Warn
        );
    }

    #[test]
    fn throttle_verdict_passes_a_quiet_unit() {
        let (status, reason) = throttle_verdict(
            MemoryEvents::default(),
            Some(0.0),
            Some(16 * 1024 * 1024 * 1024),
            Some(9 * 1024 * 1024 * 1024),
        );
        assert_eq!(status, Status::Pass, "{reason}");
        // An unthrottled unit with no cap configured (`MemoryHigh=max`)
        // passes as well — nothing to judge against.
        assert_eq!(
            throttle_verdict(MemoryEvents::default(), Some(0.0), None, None).0,
            Status::Pass
        );
    }

    #[test]
    fn throttle_check_is_registered_last_before_the_mon_planes() {
        let names: Vec<&str> = default_checks().iter().map(|c| c.name()).collect();
        assert!(
            names.contains(&"supervision.cgroup_memory_throttle"),
            "cgroup throttle check missing from the SPEC §4.6 order: {names:?}"
        );
        assert_eq!(
            names
                .iter()
                .filter(|n| **n == "supervision.cgroup_memory_throttle")
                .count(),
            1,
            "checks must be registered exactly once"
        );
    }

    #[test]
    fn compiled_partitions_only_counts_dirs_holding_a_rai() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("compiled_key_a")).expect("a");
        std::fs::write(root.join("compiled_key_a/key_a.rai"), b"rai").expect("rai");
        std::fs::create_dir_all(root.join("compiled_empty")).expect("b");
        std::fs::create_dir_all(root.join("notcompiled")).expect("c");
        std::fs::write(root.join("notcompiled/stray.rai"), b"rai").expect("stray");
        let found = compiled_partitions(&[root.to_path_buf()]);
        assert_eq!(found, vec![root.join("compiled_key_a")]);
    }

    #[test]
    fn compiled_partitions_tolerates_a_missing_model_dir() {
        assert!(compiled_partitions(&[PathBuf::from("/nonexistent/sy/aiplane/x")]).is_empty());
    }

    #[test]
    fn artifact_verdict_is_ready_with_graph_and_tokenizer() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let model = dir.path().join("m.bf16.onnx");
        let tok = dir.path().join("m.tokenizer/tokenizer.json");
        std::fs::create_dir_all(tok.parent().unwrap()).expect("tok dir");
        std::fs::write(&model, b"graph").expect("model");
        std::fs::write(&tok, b"{}").expect("tokenizer");
        assert_eq!(
            classify_artifact(&model, Some(&dir.path().join("m.onnx")), &tok),
            ArtifactVerdict::Ready
        );
    }

    #[test]
    fn artifact_verdict_is_cpu_only_when_only_the_fp32_export_survived() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let model = dir.path().join("m.bf16.onnx");
        let fp32 = dir.path().join("m.onnx");
        let tok = dir.path().join("m.tokenizer/tokenizer.json");
        std::fs::create_dir_all(tok.parent().unwrap()).expect("tok dir");
        std::fs::write(&fp32, b"graph").expect("fp32");
        std::fs::write(&tok, b"{}").expect("tokenizer");
        assert_eq!(
            classify_artifact(&model, Some(&fp32), &tok),
            ArtifactVerdict::CpuOnly,
            "warn, do not fail: embeddings still run"
        );
    }

    #[test]
    fn artifact_verdict_names_a_dangling_symlink() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let model = dir.path().join("m.bf16.onnx");
        let tok = dir.path().join("m.tokenizer/tokenizer.json");
        std::fs::create_dir_all(tok.parent().unwrap()).expect("tok dir");
        std::fs::write(&tok, b"{}").expect("tokenizer");
        std::os::unix::fs::symlink(dir.path().join("pruned-by-a-cache-sweep"), &model)
            .expect("symlink");
        match classify_artifact(&model, None, &tok) {
            ArtifactVerdict::Absent(paths) => {
                assert_eq!(paths.len(), 1);
                assert!(paths[0].contains("dangling symlink"), "{}", paths[0]);
            }
            other => panic!("expected Absent, got {other:?}"),
        }
    }

    #[test]
    fn artifact_verdict_prioritises_a_missing_tokenizer() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let model = dir.path().join("m.bf16.onnx");
        std::fs::write(&model, b"graph").expect("model");
        let tok = dir.path().join("m.tokenizer/tokenizer.json");
        assert_eq!(
            classify_artifact(&model, None, &tok),
            ArtifactVerdict::Absent(vec![format!("tokenizer.json missing at {}", tok.display())]),
            "no tokenizer means no encoding, graph or not"
        );
    }

    #[test]
    fn only_a_critical_artifact_gap_fails_the_check() {
        let preps = vec!["python prep_npu_workload.py --workload embed".to_string()];
        let fail = verdict_report(
            vec!["embed: nothing loadable".into()],
            vec!["rerank: nothing loadable".into()],
            preps.clone(),
            0,
            2,
        );
        assert_eq!(fail.status, Status::Fail);
        assert_eq!(fail.message.as_deref(), Some("embed: nothing loadable"));
        assert!(fail
            .fix
            .as_deref()
            .is_some_and(|f| f.contains("--workload embed")));

        // Same host, only the optional reranker missing -> warn.
        let warn = verdict_report(
            Vec::new(),
            vec!["rerank: nothing loadable".into()],
            preps,
            1,
            2,
        );
        assert_eq!(warn.status, Status::Warn);
    }

    #[test]
    fn ipc_unit_names_point_at_units_that_exist() {
        for ep in [
            IpcEndpoint::knowledge(),
            IpcEndpoint::aiplane(),
            IpcEndpoint::agt(),
            IpcEndpoint::stack(),
        ] {
            let unit = ep
                .unit()
                .unwrap_or_else(|| panic!("{} has no unit", ep.name()));
            let path = Path::new("configs/systemd/user").join(unit);
            assert!(path.is_file(), "{} -> {unit} missing", ep.name());
        }
        // The aiplane facade rides on the knowledge daemon's socket.
        assert_eq!(IpcEndpoint::aiplane().unit(), Some("sy-knowledge.service"));
        assert_eq!(IpcEndpoint::agt().unit(), Some("sy-agentd.service"));
    }

    #[test]
    fn missing_socket_on_an_enabled_unit_is_a_warning() {
        let ep = IpcEndpoint::knowledge();
        let sock = std::path::Path::new("/run/user/1000/sy-knowledge.sock");
        let res = ep.classify_missing(sock, Some(true));
        assert_eq!(res.status, Status::Warn);
        assert!(res
            .message
            .as_deref()
            .is_some_and(|m| m.contains("failing to start")));
        assert!(
            res.fix
                .as_deref()
                .is_some_and(|f| f.contains("sy-knowledge.service")),
            "fix must name a real unit"
        );

        assert_eq!(ep.classify_missing(sock, Some(false)).status, Status::Skip);
        assert_eq!(ep.classify_missing(sock, None).status, Status::Skip);
    }

    #[test]
    fn qdrant_version_check_classifies_body() {
        // knowledge-retrieval-iter1 cross-cutting DoD: the doctor check
        // maps a live qdrant root-body version to pass (≥1.16) / fail
        // (<1.16, with the `sy apply` hint) and tolerates an unreachable
        // qdrant (warn). The HTTP fetch is exercised end-to-end by the
        // `e2e_runs_and_emits_summary` runner test; here we pin the pure
        // classification a real `GET /` body drives.
        let ok = QdrantVersionMin.classify(Some(r#"{"version":"1.18.1"}"#.into()));
        assert_eq!(ok.status, Status::Pass);

        let old = QdrantVersionMin.classify(Some(r#"{"version":"1.12.4"}"#.into()));
        assert_eq!(old.status, Status::Fail);
        assert!(old.fix.as_deref().unwrap_or("").contains("sy apply"));
        assert!(old.message.as_deref().unwrap_or("").contains("1.12"));

        // Unreachable qdrant → warn, not fail (the daemon may simply be down;
        // `knowledge.qdrant_reachable` already covers hard reachability).
        let down = QdrantVersionMin.classify(None);
        assert_eq!(down.status, Status::Warn);
    }

    #[test]
    fn landlock_version_parses_lsm() {
        // Synthetic /sys/kernel/security/lsm content per SPEC §6 risk
        // row 4 — the kernel emits a comma-separated LSM list with no
        // ABI version, so the parser must return the `landlock` token
        // as a presence marker.
        let lsm = "capability,yama,bpf,landlock\n";
        assert_eq!(landlock_token(lsm), Some("landlock"));
    }

    #[test]
    fn landlock_version_absent_returns_none() {
        // A pre-5.13 kernel reports an LSM list without landlock; the
        // parser must signal absence so the check can report `warn`
        // with the upgrade-kernel fix-it.
        let lsm = "capability,yama,bpf\n";
        assert_eq!(landlock_token(lsm), None);
    }

    #[test]
    fn landlock_version_tolerates_whitespace() {
        let lsm = "  capability , yama ,  landlock  , bpf\n";
        assert_eq!(landlock_token(lsm), Some("landlock"));
    }

    #[test]
    fn coredumpctl_count_parses_array() {
        // The `--json=pretty` format is a JSON array of objects; the
        // parser counts entries without unpacking each one.
        let stdout = br#"[
            {"_TIME": "1"},
            {"_TIME": "2"}
        ]"#;
        assert_eq!(parse_coredumpctl_count(stdout), 2);
    }

    #[test]
    fn coredumpctl_count_handles_non_array() {
        // Non-array output (older coredumpctl, error envelope, …) must
        // degrade to zero rather than panicking.
        assert_eq!(parse_coredumpctl_count(b""), 0);
        assert_eq!(parse_coredumpctl_count(b"null"), 0);
        assert_eq!(parse_coredumpctl_count(b"not json"), 0);
    }

    #[test]
    fn active_sandbox_scopes_pass_on_empty_list() {
        // `systemctl --user list-units --type=scope --no-legend` returns
        // empty stdout when there are no scope units. The parser must
        // report zero scopes so the check can pass-with-count=0
        // (arch-agent-sandbox Step 4 final DoD bullet).
        assert_eq!(parse_scope_count(b""), 0);
        assert_eq!(parse_scope_count(b"\n"), 0);
        assert_eq!(parse_scope_count(b"   \n   \n"), 0);
    }

    #[test]
    fn active_sandbox_scopes_pass_with_count() {
        // Synthetic `systemctl --user list-units --type=scope
        // --no-legend` output (per `systemctl(1)` man page §"Output
        // format"): one unit per line, columns `UNIT LOAD ACTIVE SUB
        // DESCRIPTION`. We count non-blank lines.
        let out = b"run-12345.scope loaded active running /usr/bin/rg\n\
                    run-67890.scope loaded active running /usr/bin/cat\n\
                    app-niri-foot-11816.scope loaded active running niri foot\n";
        assert_eq!(parse_scope_count(out), 3);
    }

    #[test]
    fn active_sandbox_scopes_handles_missing_systemctl() {
        // When `systemctl` is not on PATH, the check must return
        // `Skip` rather than `Fail` — the sandbox is functional with or
        // without a user manager, and `sy doctor`'s `kernel.systemd_user_session`
        // check already flags the missing prerequisite separately.
        // Drive the path by giving `run_systemctl_list_scopes` a PATH
        // with no `systemctl` on it.
        let result = ActiveSandboxScopes.run_with_path(Some(""));
        assert_eq!(result.status, Status::Skip);
    }

    /// Lock around `XDG_RUNTIME_DIR` mutation so the per-endpoint
    /// socket-missing tests below can run in parallel under cargo's
    /// default scheduler without trampling each other's env. Use the
    /// crate-wide canonical lock so we also serialise against
    /// `aiplane::ipc::tests`, which dial sockets resolved from the
    /// same env var.
    use crate::aiplane::TEST_ENV_LOCK as IPC_ENV_LOCK;

    fn with_runtime_dir<F: FnOnce()>(dir: &std::path::Path, f: F) {
        let _guard = IPC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", dir);
        f();
        match prev {
            Some(v) => env::set_var("XDG_RUNTIME_DIR", v),
            None => env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn agt_socket_check_skips_when_path_missing() {
        // arch-ipc-v1 cross-cutting DoD: `sy doctor` must round-trip
        // `system.health` against the agt socket. When the daemon is
        // not running (socket file absent) the check skips rather than
        // hard-failing — same posture as the other IPC endpoint checks.
        let tmp = tempfile::tempdir().expect("tempdir");
        with_runtime_dir(tmp.path(), || {
            // Probe pinned: an absent socket behind a *disabled* unit is the
            // honest Skip. (Enabled + absent is a Warn — see
            // `missing_socket_on_an_enabled_unit_is_a_warning`.)
            let result = IpcEndpoint::agt().with_unit_state(|_| Some(false)).run();
            assert_eq!(result.status, Status::Skip);
        });
    }

    #[test]
    fn stack_socket_check_skips_when_path_missing() {
        // Mirror of the agt check for the stack-bar socket. The
        // resolved path is `$XDG_RUNTIME_DIR/sy/stackbar.sock`; with a
        // fresh tempdir the parent `sy/` directory doesn't exist
        // either, so the check must skip cleanly without panicking.
        let tmp = tempfile::tempdir().expect("tempdir");
        with_runtime_dir(tmp.path(), || {
            let result = IpcEndpoint::stack().with_unit_state(|_| Some(false)).run();
            assert_eq!(result.status, Status::Skip);
        });
    }
}
