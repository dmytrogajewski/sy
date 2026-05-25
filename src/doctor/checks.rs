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
        Box::new(IpcEndpoint::knowledge()),
        Box::new(IpcEndpoint::aiplane()),
        Box::new(IpcEndpoint::agt()),
        Box::new(IpcEndpoint::stack()),
        Box::new(UserUnitsPresent),
        Box::new(ActiveSandboxScopes),
        Box::new(LandlockVersion),
        Box::new(SystemdUserSession),
        Box::new(CoredumpRecentCount),
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

#[cfg(test)]
mod tests {
    use super::*;

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
