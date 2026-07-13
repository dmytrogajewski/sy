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
/// sy-mon ROADMAP Step 21 appends the dashboard-plumbing checks
/// (`mon.collect.running`, one `mon.metrics_socket.<plane>` per known
/// plane, `mon.history.writable`) so the daily `sy doctor` sweep
/// covers the popup + aggregator surface.
pub fn default_checks() -> Vec<Box<dyn Check>> {
    let mut checks: Vec<Box<dyn Check>> = vec![
        Box::new(NpuDevice),
        Box::new(VitisaiCachePresent),
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
        Box::new(TunedConflict),
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
        let dir = vitisai_cache_dir();
        match dir.as_ref().and_then(|d| count_entries(d)) {
            Some(n) if n > 0 => CheckResult {
                name: self.name(),
                status: Status::Pass,
                message: Some(format!("{n} compiled artefacts cached")),
                fix: None,
                details: Some(json!({
                    "dir": dir.as_ref().map(|d| d.display().to_string()),
                    "entries": n,
                })),
            },
            Some(_) => CheckResult {
                name: self.name(),
                status: Status::Warn,
                message: Some("vitisai compile cache exists but is empty".into()),
                fix: Some("run `sy aiplane run --workload fake` to seed the cache".into()),
                details: None,
            },
            None => CheckResult {
                name: self.name(),
                status: Status::Skip,
                message: Some("vitisai compile cache dir not present yet".into()),
                fix: None,
                details: None,
            },
        }
    }
}

fn vitisai_cache_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("sy").join("aiplane").join("compile"))
}

fn count_entries(dir: &Path) -> Option<usize> {
    let rd = fs::read_dir(dir).ok()?;
    Some(rd.filter_map(|e| e.ok()).count())
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
        use crate::knowledge::qdrant::{MIN_HYBRID_VERSION, meets_min_version, parse_version};
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
}

impl IpcEndpoint {
    pub fn knowledge() -> Self {
        Self {
            name: "ipc.knowledge_sock",
            endpoint: "knowledge",
        }
    }
    pub fn aiplane() -> Self {
        Self {
            name: "ipc.aiplane_sock",
            endpoint: "aiplane",
        }
    }
    pub fn agt() -> Self {
        Self {
            name: "ipc.agt_sock",
            endpoint: "agt",
        }
    }
    pub fn stack() -> Self {
        Self {
            name: "ipc.stack_sock",
            endpoint: "stack",
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
            return CheckResult {
                name: self.name,
                status: Status::Skip,
                message: Some(format!("{} not present", sock.display())),
                fix: Some(format!(
                    "start the daemon: `systemctl --user start sy-{}.service`",
                    self.endpoint
                )),
                details: None,
            };
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
                fix: Some(format!(
                    "start the daemon: `systemctl --user start sy-{}.service`",
                    self.endpoint
                )),
                details: None,
            },
        }
    }
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

// -- power.tuned_conflict --------------------------------------------------

/// The competing EPP / `platform_profile` writer we defend against. On
/// Fedora 43 the `tuned` package ships `tuned.service` (and the D-Bus
/// PPD shim `tuned-ppd.service`); either one rewrites the same sysfs
/// knobs `sy-powerd` actuates, silently undoing the daemon's writes.
/// Two writers for one knob is a snowflake risk (CLAUDE.md) — this
/// check surfaces it and points at the declarative remediation.
const TUNED_SERVICE: &str = "tuned.service";
const TUNED_PPD_SERVICE: &str = "tuned-ppd.service";
const SY_POWERD_SERVICE: &str = "sy-powerd.service";

/// Flags the two-writers conflict: `tuned.service` (or its
/// `tuned-ppd.service` variant) is active while `sy-powerd` is enabled,
/// so both actors race to own EPP / `platform_profile`. Read-only —
/// queries `systemctl is-active` / `is-enabled` and never mutates unit
/// state (masking is `sy power apply`'s job, gated on `--yes`).
///
/// Status mapping:
/// - `Skip` — `systemctl` not on PATH, or `sy-powerd` not enabled (no
///   ownership to defend, so a live tuned is not sy's conflict).
/// - `Fail` — `sy-powerd` enabled AND a tuned unit active: names the
///   conflict + the `sy power apply` remediation.
/// - `Pass` — `sy-powerd` enabled, no competing writer active.
pub struct TunedConflict;

impl TunedConflict {
    /// Test seam mirroring [`ActiveSandboxScopes::run_with_path`]: a
    /// `Some("")` PATH exercises the "systemctl missing" branch without
    /// mutating the process env. `None` uses the inherited PATH.
    fn run_with_path(&self, path_override: Option<&str>) -> CheckResult {
        let powerd_enabled =
            match systemctl_stdout(&["--user", "is-enabled", SY_POWERD_SERVICE], path_override) {
                Ok(out) => unit_is_enabled(&out),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return CheckResult {
                        name: self.name(),
                        status: Status::Skip,
                        message: Some("systemctl not on PATH".into()),
                        fix: None,
                        details: None,
                    };
                }
                Err(e) => {
                    return CheckResult {
                        name: self.name(),
                        status: Status::Skip,
                        message: Some(format!("systemctl failed: {e}")),
                        fix: None,
                        details: None,
                    };
                }
            };
        // `is-active` exits non-zero for an inactive unit but still
        // prints the state to stdout; a spawn failure degrades to
        // "not active" (the is-enabled probe above already skipped on a
        // missing systemctl).
        let tuned = systemctl_stdout(&["is-active", TUNED_SERVICE], path_override)
            .map(|o| unit_is_active(&o))
            .unwrap_or(false);
        let tuned_ppd = systemctl_stdout(&["is-active", TUNED_PPD_SERVICE], path_override)
            .map(|o| unit_is_active(&o))
            .unwrap_or(false);
        self.evaluate(tuned, tuned_ppd, powerd_enabled)
    }

    /// Pure conflict classifier — split out so the pass/fail/skip
    /// mapping is unit-testable without a live systemd.
    fn evaluate(&self, tuned: bool, tuned_ppd: bool, powerd_enabled: bool) -> CheckResult {
        if !powerd_enabled {
            return CheckResult {
                name: self.name(),
                status: Status::Skip,
                message: Some(format!(
                    "{SY_POWERD_SERVICE} not enabled; no EPP/platform_profile ownership to defend"
                )),
                fix: None,
                details: None,
            };
        }
        let active: Vec<&str> = [(TUNED_SERVICE, tuned), (TUNED_PPD_SERVICE, tuned_ppd)]
            .into_iter()
            .filter_map(|(name, on)| on.then_some(name))
            .collect();
        if active.is_empty() {
            CheckResult {
                name: self.name(),
                status: Status::Pass,
                message: Some(format!(
                    "no competing EPP/platform_profile writer active ({SY_POWERD_SERVICE} owns it)"
                )),
                fix: None,
                details: None,
            }
        } else {
            CheckResult {
                name: self.name(),
                status: Status::Fail,
                message: Some(format!(
                    "{} active alongside enabled {SY_POWERD_SERVICE}: two writers for EPP/platform_profile",
                    active.join(" + ")
                )),
                fix: Some(
                    "run `sy power apply` (plans disabling+masking tuned) then reboot".into(),
                ),
                details: Some(json!({ "competing_writers": active })),
            }
        }
    }
}

impl Check for TunedConflict {
    fn name(&self) -> &'static str {
        "power.tuned_conflict"
    }
    fn run(&self) -> CheckResult {
        self.run_with_path(None)
    }
}

/// Run `systemctl <args>` and return its stdout regardless of exit
/// status. `systemctl is-active` / `is-enabled` exit non-zero for
/// inactive / disabled units yet still print the state word, so we key
/// off stdout, not the exit code. Only a spawn failure (missing binary)
/// surfaces as `Err`.
fn systemctl_stdout(args: &[&str], path_override: Option<&str>) -> std::io::Result<Vec<u8>> {
    let mut cmd = Command::new(SYSTEMCTL);
    cmd.args(args);
    if let Some(p) = path_override {
        cmd.env("PATH", p);
    }
    cmd.output().map(|o| o.stdout)
}

/// True iff `systemctl is-active <unit>` reported `active`. Any other
/// word (`inactive`, `failed`, `activating`, empty) is treated as "not
/// a live writer".
fn unit_is_active(stdout: &[u8]) -> bool {
    std::str::from_utf8(stdout)
        .map(|s| s.trim() == "active")
        .unwrap_or(false)
}

/// True iff `systemctl is-enabled <unit>` reported `enabled`. `static`,
/// `disabled`, `masked`, and `linked` all mean "sy-powerd is not the
/// declared owner", so only the exact `enabled` word counts.
fn unit_is_enabled(stdout: &[u8]) -> bool {
    std::str::from_utf8(stdout)
        .map(|s| s.lines().next().unwrap_or("").trim() == "enabled")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let result = IpcEndpoint::agt().run();
            assert_eq!(result.status, Status::Skip);
        });
    }

    #[test]
    fn tuned_conflict_fails_when_tuned_active_and_powerd_enabled() {
        // Live-host shape (audit 2026-07-12): tuned.service active,
        // tuned-ppd inactive, sy-powerd enabled → two writers race for
        // EPP/platform_profile. The check must FAIL, name the conflict,
        // and point at the `sy power apply` remediation.
        let r = TunedConflict.evaluate(
            /* tuned */ true, /* tuned_ppd */ false, /* powerd */ true,
        );
        assert_eq!(r.status, Status::Fail);
        let msg = r.message.as_deref().unwrap_or("");
        assert!(msg.contains("tuned.service"), "names the unit: {msg:?}");
        assert!(
            msg.contains("two writers for EPP/platform_profile"),
            "names the conflict: {msg:?}"
        );
        assert!(
            r.fix.as_deref().unwrap_or("").contains("sy power apply"),
            "fix points at sy power apply: {:?}",
            r.fix
        );
    }

    #[test]
    fn tuned_conflict_fails_on_tuned_ppd_variant() {
        // The D-Bus PPD shim `tuned-ppd.service` writes the same knobs;
        // it must trip the same conflict even when plain tuned is down.
        let r = TunedConflict.evaluate(false, /* tuned_ppd */ true, true);
        assert_eq!(r.status, Status::Fail);
        assert!(
            r.message
                .as_deref()
                .unwrap_or("")
                .contains("tuned-ppd.service"),
            "names the tuned-ppd variant: {:?}",
            r.message
        );
    }

    #[test]
    fn tuned_conflict_passes_when_no_competing_writer() {
        // sy-powerd enabled, no tuned unit active → sy owns the knobs
        // uncontested. Pass.
        let r = TunedConflict.evaluate(false, false, true);
        assert_eq!(r.status, Status::Pass);
    }

    #[test]
    fn tuned_conflict_skips_when_powerd_not_enabled() {
        // With sy-powerd not enabled there's no ownership to defend, so
        // a live tuned is not sy's conflict — skip rather than fail.
        let r = TunedConflict.evaluate(true, true, /* powerd */ false);
        assert_eq!(r.status, Status::Skip);
    }

    #[test]
    fn tuned_conflict_skips_when_systemctl_missing() {
        // No `systemctl` on PATH → the check can't determine ownership;
        // skip cleanly (mirrors ActiveSandboxScopes). Drive the branch
        // with an empty PATH override so no real systemd is consulted.
        let r = TunedConflict.run_with_path(Some(""));
        assert_eq!(r.status, Status::Skip);
    }

    #[test]
    fn tuned_state_parsers_key_off_stdout() {
        // `systemctl is-active` / `is-enabled` exit non-zero for
        // inactive/disabled units but print the state word; the parsers
        // must key off that word, tolerating trailing newlines.
        assert!(unit_is_active(b"active\n"));
        assert!(!unit_is_active(b"inactive\n"));
        assert!(!unit_is_active(b"failed\n"));
        assert!(unit_is_enabled(b"enabled\n"));
        assert!(!unit_is_enabled(b"disabled\n"));
        assert!(!unit_is_enabled(b"masked\n"));
    }

    #[test]
    fn stack_socket_check_skips_when_path_missing() {
        // Mirror of the agt check for the stack-bar socket. The
        // resolved path is `$XDG_RUNTIME_DIR/sy/stackbar.sock`; with a
        // fresh tempdir the parent `sy/` directory doesn't exist
        // either, so the check must skip cleanly without panicking.
        let tmp = tempfile::tempdir().expect("tempdir");
        with_runtime_dir(tmp.path(), || {
            let result = IpcEndpoint::stack().run();
            assert_eq!(result.status, Status::Skip);
        });
    }
}
