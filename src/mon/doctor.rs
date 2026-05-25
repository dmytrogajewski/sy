//! `sy mon doctor` — linear-checks validation surface for the
//! sy-mon dashboard plumbing (SPEC §4 "CLI / MCP surface"; sy-mon
//! ROADMAP Step 21).
//!
//! Three families of checks, all of which also register under top-level
//! `sy doctor` via [`mon_doctor_checks`]:
//!
//! 1. [`MonCollectRunning`] (`mon.collect.running`) — IPC
//!    `system.health` probe against `$XDG_RUNTIME_DIR/sy/mon.sock`.
//!    `Fail` if the aggregator socket is missing or unresponsive — the
//!    fix message names the systemd unit (`sy-mon-collect.service`).
//! 2. [`MonMetricsSocket`] (`mon.metrics_socket.<plane>`) — one check
//!    per entry in [`super::collect::tick::KNOWN_PLANES`]; connects to
//!    `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock`, issues `GET /metrics`,
//!    parses the body via `prometheus_parse`. `Warn` (not `Fail`) when
//!    the socket is absent or unparseable — a plane that isn't running
//!    is a valid state and the aggregator tolerates it.
//! 3. [`MonHistoryWritable`] (`mon.history.writable`) — verifies the
//!    `default_history_path()` parent directory exists (or can be
//!    created) and is writable. `Fail` otherwise — without it the
//!    popup can't draw historical sparklines.
//!
//! Each check struct accepts an explicit `XDG_RUNTIME_DIR` override via
//! `with_runtime_dir(&Path)` so the unit tests below can drive the
//! whole surface against a tempdir without mutating the process env.
//! Production constructors (`MonCollectRunning::new()` etc.) read the
//! live `XDG_RUNTIME_DIR`.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use crate::doctor::{Check, CheckResult, Doctor, DoctorOpts, Status};

use super::collect::tick::KNOWN_PLANES;

/// Systemd unit name surfaced in `Fail`/`Warn` fix messages. Pulled
/// from [`super::client::AGGREGATOR_UNIT`] so the unit name is one
/// source of truth.
pub use super::client::AGGREGATOR_UNIT;

/// Per-check probe timeout. Keep short — `sy doctor` runs ~20 checks
/// serially and an unresponsive socket should not extend the doctor's
/// wall-clock by more than ~1 s total across all sy-mon checks.
const PROBE_TIMEOUT_MS: u64 = 800;

/// HTTP/1.1 `GET /metrics` literal used to scrape a plane exporter.
/// Mirrors `super::collect::scrape::SCRAPE_REQUEST` but kept local so
/// the doctor check stays decoupled from the async scraper.
const METRICS_REQUEST: &[u8] = b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
const HEADER_BODY_DELIM: &[u8] = b"\r\n\r\n";

// ── `mon.collect.running` ────────────────────────────────────────────

/// Check that the `sy mon collect` aggregator is bound to its UDS and
/// responds to `system.health`. The probe reuses the synchronous
/// length-delimited round-trip from [`crate::doctor::checks::probe_system_health`]
/// so we don't spin up a tokio runtime per check.
pub struct MonCollectRunning {
    runtime_dir: Option<PathBuf>,
}

impl MonCollectRunning {
    /// Production constructor: reads `XDG_RUNTIME_DIR` at probe time.
    pub fn new() -> Self {
        Self { runtime_dir: None }
    }

    /// Test seam: pin the runtime dir so unit tests can drive the
    /// check against a tempdir without mutating the process env. The
    /// production code path leaves this `None` and reads the env var.
    #[cfg(test)]
    pub fn with_runtime_dir(dir: &Path) -> Self {
        Self {
            runtime_dir: Some(dir.to_path_buf()),
        }
    }

    fn aggregator_socket(&self) -> Option<PathBuf> {
        let root = self
            .runtime_dir
            .clone()
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))?;
        Some(root.join("sy").join("mon.sock"))
    }
}

impl Default for MonCollectRunning {
    fn default() -> Self {
        Self::new()
    }
}

impl Check for MonCollectRunning {
    fn name(&self) -> &'static str {
        "mon.collect.running"
    }
    fn run(&self) -> CheckResult {
        let sock = match self.aggregator_socket() {
            Some(s) => s,
            None => {
                return CheckResult {
                    name: self.name(),
                    status: Status::Fail,
                    message: Some("XDG_RUNTIME_DIR unset; cannot resolve mon.sock".into()),
                    fix: Some(format!(
                        "ensure systemd user session is live and start `systemctl --user start {AGGREGATOR_UNIT}`"
                    )),
                    details: None,
                };
            }
        };
        if !sock.exists() {
            return CheckResult {
                name: self.name(),
                status: Status::Fail,
                message: Some(format!("{} not present", sock.display())),
                fix: Some(format!("start `systemctl --user start {AGGREGATOR_UNIT}`")),
                details: Some(json!({ "socket": sock.display().to_string() })),
            };
        }
        match crate::doctor::checks::probe_system_health(&sock) {
            Ok(state) => CheckResult {
                name: self.name(),
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
                name: self.name(),
                status: Status::Fail,
                message: Some(format!("{}: {e}", sock.display())),
                fix: Some(format!("start `systemctl --user start {AGGREGATOR_UNIT}`")),
                details: Some(json!({ "socket": sock.display().to_string() })),
            },
        }
    }
}

// ── `mon.metrics_socket.<plane>` ─────────────────────────────────────

/// Check that one plane's `metrics.sock` Prometheus exporter is alive
/// and serving parseable exposition. Missing socket / unparseable body
/// → `Warn` (the plane may simply not be running, which the aggregator
/// tolerates per SPEC §4 Reliability).
pub struct MonMetricsSocket {
    /// Plane name (one of `super::collect::tick::KNOWN_PLANES`). Owned
    /// `String` because we synthesise the `Check::name()` `'static` str
    /// via [`Box::leak`] at construction time so the runner can return
    /// `&'static str` without per-check allocation in the hot path.
    plane: &'static str,
    /// Leaked `mon.metrics_socket.<plane>` — see [`leak_name`].
    leaked_name: &'static str,
    runtime_dir: Option<PathBuf>,
}

impl MonMetricsSocket {
    /// Production constructor for a known plane. `plane` must be one
    /// of [`super::collect::tick::KNOWN_PLANES`]; the slice is the
    /// single source of truth so callers compose `default_checks`
    /// over it.
    pub fn new(plane: &'static str) -> Self {
        Self {
            plane,
            leaked_name: leak_name(plane),
            runtime_dir: None,
        }
    }

    /// Test seam: pin the runtime dir. Same rationale as
    /// [`MonCollectRunning::with_runtime_dir`].
    #[cfg(test)]
    pub fn with_runtime_dir(plane: &'static str, dir: &Path) -> Self {
        Self {
            plane,
            leaked_name: leak_name(plane),
            runtime_dir: Some(dir.to_path_buf()),
        }
    }

    fn metrics_socket(&self) -> Option<PathBuf> {
        let root = self
            .runtime_dir
            .clone()
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))?;
        Some(root.join("sy").join(self.plane).join("metrics.sock"))
    }
}

/// Produce the `mon.metrics_socket.<plane>` `&'static str` the doctor
/// runner expects. The leak is bounded by [`KNOWN_PLANES.len()`]
/// (6 entries) and called once per `Doctor::new()`; total leaked
/// bytes ≈ 200, well under any reasonable program-lifetime cap.
fn leak_name(plane: &str) -> &'static str {
    let s = format!("mon.metrics_socket.{plane}");
    Box::leak(s.into_boxed_str())
}

impl Check for MonMetricsSocket {
    fn name(&self) -> &'static str {
        self.leaked_name
    }
    fn run(&self) -> CheckResult {
        let sock = match self.metrics_socket() {
            Some(s) => s,
            None => {
                return CheckResult {
                    name: self.name(),
                    status: Status::Warn,
                    message: Some(format!(
                        "XDG_RUNTIME_DIR unset; cannot resolve {} metrics.sock",
                        self.plane
                    )),
                    fix: None,
                    details: None,
                };
            }
        };
        if !sock.exists() {
            return CheckResult {
                name: self.name(),
                status: Status::Warn,
                message: Some(format!(
                    "{} not present (plane {:?} likely not running)",
                    sock.display(),
                    self.plane
                )),
                fix: Some(format!(
                    "start the plane daemon (e.g. `systemctl --user start sy-{}.service`)",
                    self.plane
                )),
                details: Some(json!({
                    "plane": self.plane,
                    "socket": sock.display().to_string(),
                })),
            };
        }
        match probe_metrics_socket(&sock) {
            Ok(n_samples) => CheckResult {
                name: self.name(),
                status: Status::Pass,
                message: Some(format!("{n_samples} samples")),
                fix: None,
                details: Some(json!({
                    "plane": self.plane,
                    "socket": sock.display().to_string(),
                    "samples": n_samples,
                })),
            },
            Err(e) => CheckResult {
                name: self.name(),
                status: Status::Warn,
                message: Some(format!("{}: {e}", sock.display())),
                fix: Some(format!(
                    "check the {} exporter (was it restarted?)",
                    self.plane
                )),
                details: Some(json!({
                    "plane": self.plane,
                    "socket": sock.display().to_string(),
                })),
            },
        }
    }
}

/// Connect to `sock`, send `GET /metrics`, parse the response body.
/// Returns the count of parsed samples on success. Blocking — same
/// rationale as `crate::doctor::checks::probe_system_health` (avoid
/// spinning a tokio runtime per check).
fn probe_metrics_socket(sock: &Path) -> Result<usize, String> {
    let mut stream = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(PROBE_TIMEOUT_MS)))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_millis(PROBE_TIMEOUT_MS)))
        .map_err(|e| format!("set_write_timeout: {e}"))?;
    stream
        .write_all(METRICS_REQUEST)
        .map_err(|e| format!("write request: {e}"))?;
    let mut buf = Vec::with_capacity(4096);
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read response: {e}"))?;
    let delim = buf
        .windows(HEADER_BODY_DELIM.len())
        .position(|w| w == HEADER_BODY_DELIM)
        .ok_or_else(|| "response missing CRLFCRLF header/body delimiter".to_string())?;
    let body = &buf[delim + HEADER_BODY_DELIM.len()..];
    let text = std::str::from_utf8(body).map_err(|e| format!("non-UTF8 body: {e}"))?;
    let lines = text
        .lines()
        .map(|l| std::io::Result::Ok(l.to_string()))
        .collect::<Vec<_>>();
    let scrape = prometheus_parse::Scrape::parse(lines.into_iter())
        .map_err(|e| format!("prometheus_parse: {e}"))?;
    Ok(scrape.samples.len())
}

// ── `mon.history.writable` ───────────────────────────────────────────

/// Check that the ring-buffer history file's parent directory exists
/// (or can be created) and is writable. Without it the popup can't
/// reconstitute historical sparklines on cold start, so this is a
/// `Fail` rather than `Warn`.
pub struct MonHistoryWritable {
    runtime_dir: Option<PathBuf>,
}

impl MonHistoryWritable {
    pub fn new() -> Self {
        Self { runtime_dir: None }
    }
    #[cfg(test)]
    pub fn with_runtime_dir(dir: &Path) -> Self {
        Self {
            runtime_dir: Some(dir.to_path_buf()),
        }
    }

    fn history_path(&self) -> Option<PathBuf> {
        let root = self
            .runtime_dir
            .clone()
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))?;
        Some(root.join("sy").join("mon").join("history.bin"))
    }
}

impl Default for MonHistoryWritable {
    fn default() -> Self {
        Self::new()
    }
}

impl Check for MonHistoryWritable {
    fn name(&self) -> &'static str {
        "mon.history.writable"
    }
    fn run(&self) -> CheckResult {
        let path = match self.history_path() {
            Some(p) => p,
            None => {
                return CheckResult {
                    name: self.name(),
                    status: Status::Fail,
                    message: Some("XDG_RUNTIME_DIR unset; cannot resolve history.bin path".into()),
                    fix: Some(
                        "ensure systemd user session is live (loginctl enable-linger)".into(),
                    ),
                    details: None,
                };
            }
        };
        let parent = match path.parent() {
            Some(p) => p,
            None => {
                return CheckResult {
                    name: self.name(),
                    status: Status::Fail,
                    message: Some(format!("{} has no parent directory", path.display())),
                    fix: None,
                    details: None,
                };
            }
        };
        if let Err(e) = fs::create_dir_all(parent) {
            return CheckResult {
                name: self.name(),
                status: Status::Fail,
                message: Some(format!("create_dir_all {}: {e}", parent.display())),
                fix: Some("verify XDG_RUNTIME_DIR is writable for the current user".into()),
                details: Some(json!({ "parent": parent.display().to_string() })),
            };
        }
        // Best-effort writability probe: create + delete a sentinel
        // file. We don't touch `history.bin` itself — the aggregator
        // owns that file and a doctor probe must not race its writes.
        let sentinel = parent.join(".sy-mon-doctor-probe");
        match fs::File::create(&sentinel) {
            Ok(mut f) => {
                let _ = f.write_all(b"probe");
                let _ = fs::remove_file(&sentinel);
                CheckResult {
                    name: self.name(),
                    status: Status::Pass,
                    message: Some(format!("{} writable", parent.display())),
                    fix: None,
                    details: Some(json!({
                        "history_path": path.display().to_string(),
                        "parent": parent.display().to_string(),
                    })),
                }
            }
            Err(e) => CheckResult {
                name: self.name(),
                status: Status::Fail,
                message: Some(format!("write probe {}: {e}", sentinel.display())),
                fix: Some("verify the parent directory is writable by the current user".into()),
                details: Some(json!({ "parent": parent.display().to_string() })),
            },
        }
    }
}

// ── public surface ───────────────────────────────────────────────────

/// Compose the full list of sy-mon doctor checks: collect-running +
/// one metrics-socket check per [`KNOWN_PLANES`] entry + history
/// writability. Used by both `sy mon doctor` (via [`dispatch`]) and
/// `sy doctor`'s [`crate::doctor::checks::default_checks`].
pub fn mon_doctor_checks() -> Vec<Box<dyn Check>> {
    let mut checks: Vec<Box<dyn Check>> = Vec::with_capacity(2 + KNOWN_PLANES.len());
    checks.push(Box::new(MonCollectRunning::new()));
    for plane in KNOWN_PLANES {
        checks.push(Box::new(MonMetricsSocket::new(plane)));
    }
    checks.push(Box::new(MonHistoryWritable::new()));
    checks
}

/// `sy mon doctor [--json]` entry point. Runs only the sy-mon checks,
/// prints the report, exits with the SPEC §4.7 code (0 ok, 1 fail, 3
/// warn).
pub fn dispatch(json: bool) -> Result<()> {
    let doctor = Doctor::with_checks_public(mon_doctor_checks());
    let report = doctor.run(&DoctorOpts { json, only: None });
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if json {
        let s = serde_json::to_string_pretty(&report).context("serialise mon doctor report")?;
        writeln!(out, "{s}").context("write stdout")?;
    } else {
        crate::doctor::write_human_public(&mut out, &report).context("write stdout")?;
    }
    drop(out);
    std::process::exit(report.exit_code());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::net::{UnixListener, UnixStream as StdUnixStream};
    use std::thread;
    use std::time::Duration;

    use serde_json::json;
    use ulid::Ulid;

    use crate::doctor::{Doctor, EXIT_FAIL, EXIT_OK};

    /// Read the length-prefixed `system.health` request from a blocking
    /// `UnixStream` (the doctor probe writes 4-byte BE length followed
    /// by a JSON body), then write a `Response::Ok` with
    /// `result.state = "ready"` back to the client.
    fn serve_one_system_health(mut conn: StdUnixStream) {
        let mut len_buf = [0u8; 4];
        if conn.read_exact(&mut len_buf).is_err() {
            return;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        if conn.read_exact(&mut body).is_err() {
            return;
        }
        let req: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return,
        };
        // Match the `sy_ipc::envelope::Response::Ok` shape that
        // `probe_system_health` decodes (it inspects `result.state`).
        let resp = json!({
            "schema_version": 1,
            "request_id": req.get("request_id").cloned().unwrap_or_else(|| json!(Ulid::new().to_string())),
            "result": {
                "state": "ready",
                "status_line": "ok",
                "queue_depth": 0,
                "warm_models": [],
            },
        });
        let bytes = serde_json::to_vec(&resp).expect("encode resp");
        let n = u32::try_from(bytes.len()).expect("response fits u32");
        let _ = conn.write_all(&n.to_be_bytes());
        let _ = conn.write_all(&bytes);
    }

    /// Spin up a fake aggregator at `$runtime/sy/mon.sock` serving one
    /// `system.health` round-trip. Returns the join handle so the
    /// caller can `.join()` after the probe to avoid leaking threads.
    fn spawn_fake_aggregator(runtime: &Path) -> thread::JoinHandle<()> {
        let sock_dir = runtime.join("sy");
        fs::create_dir_all(&sock_dir).expect("mkdir sy/");
        let sock = sock_dir.join("mon.sock");
        let listener = UnixListener::bind(&sock).expect("bind mon.sock");
        thread::spawn(move || {
            if let Ok((conn, _)) = listener.accept() {
                serve_one_system_health(conn);
            }
        })
    }

    /// Canned Prometheus exposition from the existing fixture so the
    /// per-plane "all good" probe sees real catalogued metric names.
    const AIPLANE_FIXTURE: &str = include_str!("../../tests/fixtures/mon/prom/aiplane/metrics.txt");

    /// Spin up a fake `metrics.sock` at
    /// `$runtime/sy/<plane>/metrics.sock` that serves a canned HTTP/1.1
    /// 200 response with the prom-exposition body. Returns the join
    /// handle so the test can `.join()` it.
    fn spawn_fake_plane_metrics(runtime: &Path, plane: &str) -> thread::JoinHandle<()> {
        let sock_dir = runtime.join("sy").join(plane);
        fs::create_dir_all(&sock_dir).expect("mkdir plane dir");
        let sock = sock_dir.join("metrics.sock");
        let listener = UnixListener::bind(&sock).expect("bind metrics.sock");
        let body = AIPLANE_FIXTURE.to_string();
        thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                // Drain the request so the client's write completes.
                let mut buf = [0u8; 1024];
                let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = conn.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = conn.write_all(response.as_bytes());
                let _ = conn.shutdown(std::net::Shutdown::Both);
            }
        })
    }

    /// Build a fresh tempdir as the synthetic runtime root for one
    /// test. Returned tempdir is held by the caller so the dir doesn't
    /// vanish mid-probe.
    fn runtime_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn passes_on_healthy_host() {
        // Step 21 spec: all sockets up → all checks `Pass`, doctor
        // exit code `EXIT_OK`. We stand up a fake aggregator + a fake
        // plane metrics socket for every `KNOWN_PLANES` entry, then
        // run the full check list through the `Doctor` runner with the
        // tempdir-pinned constructors.
        let tmp = runtime_root();
        let runtime = tmp.path();

        let agg = spawn_fake_aggregator(runtime);
        let mut plane_threads: Vec<thread::JoinHandle<()>> = KNOWN_PLANES
            .iter()
            .map(|p| spawn_fake_plane_metrics(runtime, p))
            .collect();

        // Build the production check graph but pinned at our tempdir.
        let mut checks: Vec<Box<dyn Check>> = Vec::new();
        checks.push(Box::new(MonCollectRunning::with_runtime_dir(runtime)));
        for plane in KNOWN_PLANES {
            checks.push(Box::new(MonMetricsSocket::with_runtime_dir(plane, runtime)));
        }
        checks.push(Box::new(MonHistoryWritable::with_runtime_dir(runtime)));

        let doctor = Doctor::with_checks_public(checks);
        let report = doctor.run(&DoctorOpts::default());

        // Reap server threads after the probe so they don't leak.
        agg.join().expect("aggregator thread");
        while let Some(h) = plane_threads.pop() {
            h.join().expect("plane thread");
        }

        // All checks must pass. We assert per-check so a single
        // failure surfaces an actionable name, not just a global tally.
        for c in &report.checks {
            assert_eq!(
                c.status,
                Status::Pass,
                "check {:?} expected Pass but got {:?}: msg={:?}",
                c.name,
                c.status,
                c.message,
            );
        }
        assert_eq!(report.summary.fail, 0, "fail count must be 0");
        assert_eq!(report.summary.warn, 0, "warn count must be 0");
        // SPEC §4.7 exit code: all-pass → 0.
        assert_eq!(report.exit_code(), EXIT_OK);

        // All-green JSON: serialise the full report and assert the
        // summary tally + version stamp.
        let v: serde_json::Value = serde_json::to_value(&report).expect("serialise report");
        assert_eq!(v["version"], serde_json::Value::from(1));
        assert_eq!(v["summary"]["fail"], serde_json::Value::from(0));
        assert_eq!(v["summary"]["warn"], serde_json::Value::from(0));
        let expected_total = 1 + (KNOWN_PLANES.len() as u64) + 1;
        assert_eq!(
            v["summary"]["pass"],
            serde_json::Value::from(expected_total),
        );
    }

    #[test]
    fn fails_when_collect_down() {
        // Step 21 spec: no aggregator → `mon.collect.running` emits
        // `Fail` and the fix message names the systemd unit so an
        // operator (or agent reading stderr) has a one-paste fix.
        let tmp = runtime_root();
        // Pre-create the `sy/` subdir but DO NOT bind `mon.sock` so
        // the check exercises the "socket file absent" branch.
        fs::create_dir_all(tmp.path().join("sy")).expect("mkdir sy/");

        let check = MonCollectRunning::with_runtime_dir(tmp.path());
        let result = check.run();

        assert_eq!(result.status, Status::Fail, "no aggregator must Fail");
        let fix = result.fix.unwrap_or_default();
        assert!(
            fix.contains("sy-mon-collect.service"),
            "fix must name the systemd unit; got {fix:?}",
        );

        // The Doctor runner must surface the Fail in the summary and
        // exit with EXIT_FAIL.
        let doctor = Doctor::with_checks_public(vec![Box::new(
            MonCollectRunning::with_runtime_dir(tmp.path()),
        )]);
        let report = doctor.run(&DoctorOpts::default());
        assert_eq!(report.summary.fail, 1);
        assert_eq!(report.exit_code(), EXIT_FAIL);
    }

    #[test]
    fn warns_on_missing_plane_socket() {
        // Step 21 spec: one plane offline → its metrics-socket check
        // `Warn`s, others still `Pass`. We stand up an aiplane socket
        // but leave knowledge unbound, then assert the warn/pass split.
        let tmp = runtime_root();
        let runtime = tmp.path();

        let aiplane_thread = spawn_fake_plane_metrics(runtime, "aiplane");

        let aiplane_check = MonMetricsSocket::with_runtime_dir("aiplane", runtime);
        let knowledge_check = MonMetricsSocket::with_runtime_dir("knowledge", runtime);

        let aiplane_result = aiplane_check.run();
        let knowledge_result = knowledge_check.run();

        aiplane_thread.join().expect("aiplane thread");

        assert_eq!(
            aiplane_result.status,
            Status::Pass,
            "aiplane socket up; expected Pass, got {:?} msg={:?}",
            aiplane_result.status,
            aiplane_result.message,
        );
        assert_eq!(aiplane_result.name, "mon.metrics_socket.aiplane");

        assert_eq!(
            knowledge_result.status,
            Status::Warn,
            "knowledge socket down; expected Warn, got {:?}",
            knowledge_result.status,
        );
        assert_eq!(knowledge_result.name, "mon.metrics_socket.knowledge");
        let msg = knowledge_result.message.unwrap_or_default();
        assert!(
            msg.contains("knowledge"),
            "warn message must name the plane; got {msg:?}",
        );
    }

    #[test]
    fn history_writable_passes_on_clean_tempdir() {
        // Companion smoke test for `mon.history.writable`: an empty
        // tempdir is writable, so the check creates the missing
        // `sy/mon/` parent and reports `Pass`.
        let tmp = runtime_root();
        let check = MonHistoryWritable::with_runtime_dir(tmp.path());
        let result = check.run();
        assert_eq!(
            result.status,
            Status::Pass,
            "fresh tempdir must be writable; got {:?} msg={:?}",
            result.status,
            result.message,
        );
        // The check must have created the `sy/mon/` parent so the
        // aggregator's first `mmap` doesn't ENOENT.
        let parent = tmp.path().join("sy").join("mon");
        assert!(parent.is_dir(), "history parent dir must exist after probe");
    }

    #[test]
    fn metrics_socket_name_includes_plane() {
        // Pin the `<plane>` suffix convention so a future rename of
        // the dot-separated naming scheme breaks the test, not the
        // doctor JSON consumers.
        let check = MonMetricsSocket::new("aiplane");
        assert_eq!(check.name(), "mon.metrics_socket.aiplane");
    }
}
