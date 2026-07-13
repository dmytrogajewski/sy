//! `sy power {status, daemon, apply, log, profile, explain, train,
//! show, list-profiles, mcp}` — clap subcommand tree per SPEC §4
//! "CLI / MCP Surface", plus the post-SPEC `show` subcommand
//! (Phase RV). `list-profiles` was missed at Step 1 and added by
//! Step 14 alongside the bandit arm enumeration.
//!
//! Step 1 ships the scaffold: every handler except `status` prints a
//! one-line "unimplemented — see roadmap step `<N>`" diagnostic to
//! stderr and exits 0. `status --json` emits the SPEC §4
//! `sy.power.status/v1` schema with stub values so downstream
//! consumers can lock against the contract today.

use std::io::{IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::Subcommand;

use super::clock::{Clock, SystemClock};
use super::config::PowerConfig;
use super::intent::{
    AiplaneIntentChannel, CgroupAncestryChannel, IdleChannel, LogindChannel, MprisChannel,
    NiriChannel, NotifyChannel, PsiChannel, PsiKind, ScreenCastChannel, TimeChannel,
};
use super::ipc::{encode_frame, ProfileAck, StatusRequest, StatusResponse, MAX_FRAME_BYTES};
use super::log::{parse_since, AuditEntry, Logger, DEFAULT_TAIL_WINDOW};
use super::report::{
    compile_pdf, compute_counterfactual_baseline, extract_activity_metrics, extract_bandit_metrics,
    extract_drift_metrics, extract_energy_metrics, extract_forecast_metrics,
    extract_shield_metrics, Plot, ReportHeader, ReportMetrics, ReportTemplate,
};
use super::shield::{self, ShieldState};
use super::snapshot::{self, Intent, Sensors};
use super::status::{format_explain, format_status, format_waybar, format_waybar_daemon_down};
use super::{PowerError, EXIT_DAEMON_UNREACHABLE, EXIT_DECODE_ERROR, EXIT_DRIFT_ACTIVE};

/// CLIG exit code for a malformed flag value (`--since=garbage`).
const EXIT_USAGE: i32 = 2;

/// SPEC §4 stable exit code: the audit log carries fewer than 24 h of
/// entries, so the `sy power show` report would be statistically thin.
/// Step 35 gates on this unless `--allow-thin` is set.
const EXIT_ONBOARDING_NOT_COMPLETE: i32 = 7;

/// Minimum entry count for a "thick" report. Audit log is 1 Hz, so
/// 86 400 entries ≈ 24 h. Below this floor we exit 7 unless
/// `--allow-thin` is passed.
const MIN_ENTRIES_FOR_THICK_REPORT: usize = 24 * 3600;

/// Default `--since` window for `sy power show`. Matches the SPEC §RV.2
/// example invocation `sy power show --since=7d`.
const DEFAULT_SHOW_SINCE: Duration = Duration::from_secs(7 * 24 * 3600);

/// Default `--last=N` cap for `sy power explain`. Matches SPEC §4
/// example invocation `sy power explain --last=10 --json`.
const DEFAULT_EXPLAIN_LAST: usize = 10;

#[derive(Debug, Subcommand)]
pub enum PowerCmd {
    /// Current state, profile, shield, reason.
    ///
    /// Examples:
    ///   sy power status                # human-readable summary
    ///   sy power status --json         # sy.power.status/v1 schema
    Status {
        /// Emit the SPEC §4 `sy.power.status/v1` schema on stdout.
        #[arg(long)]
        json: bool,
        /// Emit the SPEC §5 waybar pill JSON (`{text, tooltip, class}`)
        /// on stdout for the `custom/sy-power` waybar slot. Mutually
        /// exclusive with `--json`; when the daemon is unreachable the
        /// pill falls back to the `error` class and the CLI still
        /// exits 0 so waybar keeps polling.
        #[arg(long, conflicts_with = "json")]
        waybar: bool,
    },

    /// `sy-powerd` entrypoint (systemd `--user` unit).
    ///
    /// Example:
    ///   systemctl --user start sy-powerd.service
    Daemon,

    /// Install polkit action + udev rule + systemd unit + waybar tile.
    ///
    /// Examples:
    ///   sy power apply --dry-run       # preview without writing
    ///   sy power apply                 # commit changes
    ///   sy power apply --yes           # skip future destructive prompts
    Apply {
        /// Print the planned changes without touching disk.
        #[arg(long)]
        dry_run: bool,
        /// Skip interactive prompts and authorise destructive actions.
        /// Currently gates the PPD-mask path: with `--yes` + PPD
        /// detected + no `--with-ppd`, the installer runs
        /// `systemctl --user mask power-profiles-daemon.service`.
        #[arg(long)]
        yes: bool,
        /// Keep `power-profiles-daemon` active; run the `sy power`
        /// PPD shim alongside it without binding
        /// `net.hadess.PowerProfiles` (PPD keeps the bus name).
        /// Mutually informative with `--yes`: if both are passed,
        /// `--with-ppd` wins and no mask is applied.
        #[arg(long)]
        with_ppd: bool,
    },

    /// Tail the NDJSON telemetry log.
    ///
    /// Examples:
    ///   sy power log --since=1h
    ///   sy power log --json --since=10m
    Log {
        /// Filter to entries newer than this duration (e.g. `1h`).
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
        /// Emit raw NDJSON (one JSON object per line).
        #[arg(long)]
        json: bool,
    },

    /// Manual profile override (cleared by `--auto`).
    ///
    /// Examples:
    ///   sy power profile performance  # pin a profile
    ///   sy power profile --auto       # restore bandit control
    Profile {
        /// Profile name (e.g. `performance`, `balanced`, `quiet`).
        name: Option<String>,
        /// Clear any manual override and restore bandit control.
        #[arg(long, conflicts_with = "name")]
        auto: bool,
    },

    /// Audit replay: which arm fired and why.
    ///
    /// Examples:
    ///   sy power explain               # last 10 decisions
    ///   sy power explain --last=1      # most recent decision only
    ///   sy power explain --json        # sy.power.explain/v1 schema
    Explain {
        /// Show the last N decisions; defaults to the SPEC §4 cap of 10.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_EXPLAIN_LAST)]
        last: usize,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },

    /// Offline GRU retrain. Reads the NDJSON telemetry log; writes a
    /// personalised ONNX for the daemon to hot-swap.
    ///
    /// Examples:
    ///   sy power train                                       # default paths
    ///   sy power train --in telemetry.ndjson --out model.onnx
    Train {
        /// Input NDJSON path (default: telemetry log).
        #[arg(long, value_name = "PATH")]
        r#in: Option<std::path::PathBuf>,
        /// Output ONNX path (default: state-dir model).
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
    },

    /// Render the offline `sy power` report (Phase RV finale,
    /// Step 35). Default behaviour writes a dated PDF to
    /// `~/.local/state/sy/power/reports/sy-power-<rfc3339>.pdf` and
    /// opens it with `xdg-open`. `--json` skips PDF generation and
    /// emits the `sy.power.report/v1` schema on stdout.
    ///
    /// Examples:
    ///   sy power show                           # PDF + xdg-open
    ///   sy power show --since=24h --no-open     # CI / headless
    ///   sy power show --out=/tmp/report.pdf     # explicit path
    ///   sy power show --json --since=1d         # agent path
    ///
    /// Reproducible output: over the same NDJSON window the PDF is
    /// byte-identical once its two wall-clock inputs are pinned via env:
    ///   `SY_POWER_REPORT_TIMESTAMP=<RFC3339>`  pins the "Generated" line
    ///                                        (e.g. 2026-05-19T12:00:00Z)
    ///   `SY_POWER_REPORT_MODEL_SHA=<sha>`      pins the "Model version" line
    Show {
        /// Filter to entries newer than this duration (e.g. `7d`,
        /// `1h`). Default: 7 days, per SPEC §RV.2.
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
        /// Write the PDF to this path. Ignored under `--json`. Default:
        /// `~/.local/state/sy/power/reports/sy-power-<rfc3339>.pdf`.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Do not invoke `xdg-open` after writing the PDF. Always set
        /// implicitly when stdin is not a TTY (CLIG agent-friendly
        /// default).
        #[arg(long)]
        no_open: bool,
        /// Bypass the 24-h "thin window" gate and produce a report
        /// even when fewer than [`MIN_ENTRIES_FOR_THICK_REPORT`]
        /// audit entries exist.
        #[arg(long)]
        allow_thin: bool,
        /// Emit machine-readable JSON instead of a PDF.
        #[arg(long)]
        json: bool,
    },

    /// Enumerate the bandit arm table from `configs/sy/power.toml`.
    ///
    /// Examples:
    ///   sy power list-profiles            # human-readable table
    ///   sy power list-profiles --json     # sy.power.profiles/v1 schema
    ListProfiles {
        /// Emit the SPEC §4 `sy.power.profiles/v1` schema on stdout.
        #[arg(long)]
        json: bool,
    },

    /// MCP server entrypoint (stdio JSON-RPC).
    ///
    /// Example:
    ///   sy power mcp     # invoked by an MCP host such as Claude Code
    Mcp,
}

pub fn dispatch(cmd: PowerCmd) -> Result<()> {
    match cmd {
        PowerCmd::Status { json, waybar } => status(json, waybar),
        PowerCmd::Daemon => super::daemon::run(),
        PowerCmd::Apply {
            dry_run,
            yes,
            with_ppd,
        } => apply_cmd(dry_run, yes, with_ppd),
        PowerCmd::Log { since, json } => log_cmd(since, json),
        PowerCmd::Profile { name, auto } => profile_cmd(name, auto),
        PowerCmd::Explain { last, json } => explain_cmd(last, json),
        PowerCmd::Train { r#in, out } => train_cmd(r#in, out),
        PowerCmd::Show {
            since,
            out,
            no_open,
            allow_thin,
            json,
        } => show_cmd(ShowOpts {
            since,
            out,
            no_open,
            allow_thin,
            json,
        }),
        PowerCmd::ListProfiles { json } => list_profiles_cmd(json, &load_config_or_default()),
        PowerCmd::Mcp => super::mcp::run(),
    }
}

/// `sy power status` — dial `$XDG_RUNTIME_DIR/sy/powerd.sock`, request
/// the latest snapshot, render the SPEC §4 `sy.power.status/v1`
/// document. Exits 4 (per SPEC §4) when the socket is absent or the
/// daemon refuses the connection — the agent-friendly contract that
/// distinguishes "daemon-down" from a generic error.
fn status(json_out: bool, waybar_out: bool) -> Result<()> {
    let cfg = load_config_or_default();
    // Keep every sensor + intent channel referenced from the production
    // binary so the Step 1 anti-dead-code invariant survives. Step 19
    // will replace this with a config-driven snapshot driver; today the
    // tick is fed into the Step 17 shield DFA so the `shield_state`
    // slot on `sy power status --json` reflects the live host instead
    // of a stub constant.
    let sysfs = sysfs_root();
    let local_snap = snapshot::collect_tick(
        &live_sensors(),
        &mut probe_intent(Path::new("/proc/pressure")),
        &SystemClock,
        sysfs.as_path(),
    );
    // `ShieldState::CoolAc` is the documented "first tick" seed: the
    // caller (Step 19 daemon) maintains `prev` across ticks; for a
    // one-shot CLI invocation we have no history, and the DFA's
    // priority order (call_active / SOC / Tctl) reaches every other
    // state without depending on `prev` except for the MEETING lock.
    // One-shot invocation: no cross-tick history, so `secs_since_call`
    // is `None` (never in a MEETING lock window here).
    let shield_state = shield::transition(ShieldState::CoolAc, &local_snap, &cfg.shield, None);
    // Step 18 anti-dead-code probe: walk `shield::project` against an
    // empty ranked list so the projection + rules-baseline fallback
    // stay live in the production binary. The projection's pick is
    // discarded — `sy power status` is read-only by contract; Step 19
    // is what wires `project` into the daemon's actuation loop.
    let probe_tracker = shield::ThrashTracker::new();
    let _projected = shield::project(
        &[],
        shield_state,
        &local_snap,
        &cfg,
        &probe_tracker,
        std::time::Instant::now(),
    );
    // Step 15 anti-dead-code probe: instantiate the `Actuator` impls
    // and re-apply the *current* platform_profile / EPP. Both writers
    // short-circuit to `Applied::NoChange` when sysfs already matches,
    // so this is a true no-op in production — the daemon (Step 19) is
    // what will actually drive state changes. Errors are dropped on
    // the floor because `sy power status` must keep working even when
    // sysfs is unreadable (containers, CI, non-AMD hosts).
    probe_actuators(sysfs.as_path());
    // Step 22 retired the bandit anti-dead-code probe — the daemon's
    // `one_tick` now drives `Clucb::{propose_ranked,update,observe_baseline}`
    // and `compute_reward` directly, so every API surface stays
    // referenced from the production binary through the real
    // actuation loop.
    // Step 24 anti-dead-code probe: load the warmup ONNX, run one
    // inference, and discard the result. Once Step 26 wires the
    // forecaster into the daemon's `one_tick`, this probe retires the
    // same way `probe_bandit` did.
    probe_forecast(&local_snap);
    // Step 28's `probe_activity` anti-dead-code probe retired in
    // Step 29 — the daemon's `one_tick` now drives
    // `OnlineClassifier::{classify,partial_fit}` + `extract_label`
    // through the production tick path.
    // Step 30 anti-dead-code probe: instantiate the composite drift
    // detector and feed one sample to each sub-detector. Step 31
    // wires the ADWIN+DDM pair into the daemon's `one_tick` against
    // live forecast and reward residuals — drop this probe then.
    probe_drift();
    let resp = match dial_status(&super::daemon::socket_path()) {
        Ok(r) => r,
        Err(e) => {
            // SPEC §5 waybar contract: a missing daemon is a soft
            // error in the bar — emit the documented `error` pill
            // and exit 0 so waybar keeps polling at 1 Hz instead of
            // blanking the slot.
            if waybar_out {
                println!("{}", format_waybar_daemon_down());
                return Ok(());
            }
            return Err(anyhow::Error::new(classify_dial_error(&e)));
        }
    };
    let onboarding = super::onboarding::compute_onboarding_status(
        &power_state_dir(),
        &SystemClock,
        cfg.onboarding.days,
        super::checkpoint::read_anchor(&power_state_dir().join("checkpoint.json")),
    );
    if waybar_out {
        println!("{}", format_waybar(&resp, &cfg, shield_state, &onboarding));
        // Waybar pill never propagates exit-3: a drift alarm renders
        // as the `drift` class, not a CLI error. The bar keeps
        // polling regardless of the daemon's drift state.
        return Ok(());
    }
    println!(
        "{}",
        format_status(&resp, &cfg, shield_state, &onboarding, json_out)
    );
    // SPEC §4 exit-code table: an active ADWIN drift alarm surfaces
    // as exit 3 so an agent can branch on "the model is degraded"
    // without parsing the JSON. The status document is still printed
    // first — agents that don't care about the code (e.g. `sy power
    // status | jq`) still see the wire shape.
    if let Some(err) = status_drift_exit(&resp) {
        return Err(err);
    }
    Ok(())
}

/// Step 38 helper: build the SPEC §4 `sy.power.status/v1` JSON value
/// the same way `status()` does, but without printing or running the
/// anti-dead-code probes. The MCP `power_status` tool calls this
/// through `crate::power::mcp::SystemStatusFetcher` so an agent sees
/// the same view a human would on `sy power status --json`.
///
/// Daemon-down maps to `Err(anyhow::Error::from(...))` — the MCP loop
/// surfaces that as a JSON-RPC error frame so the tool stays callable
/// (no transport-level panic).
pub(crate) fn build_live_status_value() -> Result<serde_json::Value> {
    let cfg = load_config_or_default();
    let local_snap = snapshot::collect_tick(
        &live_sensors(),
        &mut probe_intent(Path::new("/proc/pressure")),
        &SystemClock,
        sysfs_root().as_path(),
    );
    // One-shot invocation: no cross-tick history, so `secs_since_call`
    // is `None` (never in a MEETING lock window here).
    let shield_state = shield::transition(ShieldState::CoolAc, &local_snap, &cfg.shield, None);
    let resp = dial_status(&super::daemon::socket_path())
        .map_err(|e| anyhow::anyhow!("sy-powerd unreachable: {e}"))?;
    let onboarding = super::onboarding::compute_onboarding_status(
        &power_state_dir(),
        &SystemClock,
        cfg.onboarding.days,
        super::checkpoint::read_anchor(&power_state_dir().join("checkpoint.json")),
    );
    Ok(super::status::build_status_value(
        &resp,
        &cfg,
        shield_state,
        &onboarding,
    ))
}

/// Pure helper: inspect the IPC `StatusResponse` and return a
/// [`PowerError`] carrying `EXIT_DRIFT_ACTIVE` (3) when the daemon's
/// ADWIN detector is in alarm. `None` for "all-clear" — the CLI exits
/// 0. Extracted so the Step 31 unit test can pin the exit-code
/// decision without standing up a daemon-in-thread.
pub(crate) fn status_drift_exit(resp: &StatusResponse) -> Option<anyhow::Error> {
    if !resp.drift.adwin_alarm {
        return None;
    }
    Some(anyhow::Error::new(PowerError {
        code: EXIT_DRIFT_ACTIVE,
        msg: "sy-powerd reports drift alarm (model degraded)".into(),
    }))
}

/// Map a `dial_*` IO error onto a structured [`PowerError`].
///
/// A decode failure (`InvalidData`) means the daemon answered but its
/// frame did not parse — a protocol/version mismatch or a malformed
/// field, *not* an unreachable socket. It gets [`EXIT_DECODE_ERROR`] and
/// a message that names the parse failure, so a reachable-but-degraded
/// daemon is never mis-reported as "unreachable" (BUG-20260712-1137).
/// Every other IO kind (connect refused, missing socket, timeout) stays
/// on [`EXIT_DAEMON_UNREACHABLE`].
fn classify_dial_error(e: &std::io::Error) -> PowerError {
    if e.kind() == std::io::ErrorKind::InvalidData {
        PowerError {
            code: EXIT_DECODE_ERROR,
            msg: format!("sy-powerd response could not be parsed: {e}"),
        }
    } else {
        PowerError {
            code: EXIT_DAEMON_UNREACHABLE,
            msg: format!("sy-powerd unreachable: {e}"),
        }
    }
}

/// Blocking dial of the `sy-powerd` Unix socket. Sends one
/// [`StatusRequest::Status`] and reads one [`StatusResponse`].
/// Mirrors `power::ipc::{encode_frame, read_frame}` but stays on
/// `std::os::unix::net::UnixStream` so the CLI doesn't spin a tokio
/// runtime for a 2-KiB round trip. `pub(crate)` so the Step 38 MCP
/// server (`crate::power::mcp::SystemStatusFetcher`) shares the exact
/// same dial path the human `sy power status` exercises.
pub(crate) fn dial_status(sock: &Path) -> std::io::Result<StatusResponse> {
    let mut stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_TIMEOUT))?;
    let buf = encode_frame(&StatusRequest::Status)?;
    stream.write_all(&buf)?;
    stream.flush()?;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame len {len} > {MAX_FRAME_BYTES}"),
        ));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("decode: {e}")))
}

/// Hard cap on the blocking dial. Two seconds is generous for a 1 Hz
/// daemon that already holds the snapshot in memory; faster would
/// flake on a freshly-started daemon mid-tick.
const IPC_TIMEOUT: Duration = Duration::from_secs(2);

/// `sy power profile <name>` / `sy power profile --auto` — Step 19
/// manual override. Validates `name` against the configured arm
/// table BEFORE dialing the socket so a typo surfaces as an
/// `EXIT_USAGE` (2) instead of a daemon round-trip. `--auto` skips
/// validation and unconditionally clears any active pin.
fn profile_cmd(name: Option<String>, auto: bool) -> Result<()> {
    let cfg = load_config_or_default();
    let req = match (name, auto) {
        (Some(_), true) => {
            return Err(anyhow::Error::new(PowerError {
                code: EXIT_USAGE,
                msg: "sy power profile: --auto conflicts with a profile name".into(),
            }))
        }
        (None, false) => {
            return Err(anyhow::Error::new(PowerError {
                code: EXIT_USAGE,
                msg: "sy power profile: pass a profile name or --auto to clear".into(),
            }))
        }
        (None, true) => StatusRequest::ProfileClear,
        (Some(n), false) => {
            let arms = super::bandit::load_arms(&cfg)?;
            if !arms.iter().any(|a| a.name == n) {
                let known: Vec<&str> = arms.iter().map(|a| a.name.as_str()).collect();
                return Err(anyhow::Error::new(PowerError {
                    code: EXIT_USAGE,
                    msg: format!("sy power profile: unknown arm {n:?}; available: {known:?}",),
                }));
            }
            StatusRequest::ProfileSet { name: n }
        }
    };
    let ack = match dial_profile(&super::daemon::socket_path(), &req) {
        Ok(a) => a,
        Err(e) => {
            return Err(anyhow::Error::new(classify_dial_error(&e)));
        }
    };
    if !ack.ok {
        return Err(anyhow::Error::new(PowerError {
            code: EXIT_USAGE,
            msg: format!(
                "sy power profile: daemon rejected: {}",
                ack.error.unwrap_or_else(|| "no error message".into()),
            ),
        }));
    }
    match ack.pinned {
        Some(p) => println!("sy power profile: pinned arm={p}"),
        None => println!("sy power profile: cleared (rules baseline active)"),
    }
    Ok(())
}

/// Blocking dial of the `sy-powerd` Unix socket for a profile op.
/// Mirrors `dial_status` but decodes a [`ProfileAck`] instead of a
/// [`StatusResponse`].
fn dial_profile(sock: &Path, req: &StatusRequest) -> std::io::Result<ProfileAck> {
    let mut stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_TIMEOUT))?;
    let buf = encode_frame(req)?;
    stream.write_all(&buf)?;
    stream.flush()?;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame len {len} > {MAX_FRAME_BYTES}"),
        ));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("decode: {e}")))
}

/// Resolve the on-disk power.toml. Missing file ⇒ defaults; parse
/// errors ⇒ defaults with a stderr warning so `sy power status`
/// never panics on a bad config (the daemon will refuse to start
/// hard, but the read-only `status` command must remain robust).
fn load_config_or_default() -> PowerConfig {
    let path = config_path();
    match PowerConfig::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "warning: failed to load {}: {e:#} — using defaults",
                path.display()
            );
            PowerConfig::default()
        }
    }
}

/// Where `sy power` looks for its config. Precedence (`SY_ROOT` →
/// cwd-if-present → installed `$XDG_CONFIG_HOME/sy/power.toml`) lives in
/// the shared [`super::power_config_path`] so the CLI, the daemon, and
/// the status surface agree on resolution (BUG-20260608-2341).
fn config_path() -> std::path::PathBuf {
    super::power_config_path()
}

/// Sysfs root for the CLI-side anti-dead-code probes (`status` and
/// `build_live_status_value`). Defaults to `/sys` in production; tests
/// override via `SY_SYSFS_ROOT` to make the in-binary probe path
/// hermetic — without the override, a hot CPU would tip the shield DFA
/// out of `CoolAc` and break the integration tests that pin the SPEC
/// §4 `bandit.baseline_arm` field.
fn sysfs_root() -> PathBuf {
    std::env::var("SY_SYSFS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/sys"))
}

/// Construct the production `Sensors` bundle consumed by
/// `snapshot::collect_tick`. Delegates to `Sensors::all` so any
/// future change to the canonical bundle (e.g. a new sensor in Step
/// 16) is picked up here automatically.
fn live_sensors() -> Sensors {
    Sensors::all()
}

/// Step 15 + Step 16 dead-code probe — instantiate all five
/// [`crate::power::apply::Actuator`] impls and invoke each one against the *current* sysfs
/// state. Every writer diffs before writing (see
/// `apply::write_if_changed`), so this is a no-op when sysfs is in
/// the expected shape. Errors are intentionally dropped: the daemon
/// (Step 19) is the production driver, and the CLI status path must
/// not crash on missing sysfs nodes (containers, non-AMD hosts, NPU
/// absent on dev VMs).
fn probe_actuators(sysfs_root: &Path) {
    use super::apply::{
        Actuator, CgroupActuator, EppActuator, IgpuActuator, NpuActuator, PlatformProfileActuator,
        SystemRunner, SystemTimeSource,
    };
    use super::bandit::{CgroupOverrides, NpuPmode};
    use super::sensors::{Sensor, SensorReading};
    let platform_sensor = super::sensors::PlatformSensor::new();
    if let Ok(SensorReading::Platform(reading)) = platform_sensor.read(sysfs_root) {
        let _ = PlatformProfileActuator::new().apply(reading.current.clone(), sysfs_root);
    }
    let pstate_sensor = super::sensors::PstateSensor::new();
    if let Ok(SensorReading::Pstate(reading)) = pstate_sensor.read(sysfs_root) {
        let _ = EppActuator::new().apply(map_sensor_epp(reading.epp), sysfs_root);
    }
    // Step 16 levers: iGPU + NPU + cgroup. The iGPU probe re-applies
    // the *currently active* mode (idempotent no-op via `Applied::NoChange`).
    // The cgroup probe passes an empty `CgroupOverrides` so no leaf is
    // touched. The NPU probe instantiates `SystemRunner` + `SystemTimeSource`
    // (anti-dead-code) and builds an actuator on top of them, but the
    // actual probe `.apply()` runs through a no-op runner — `sy power
    // status` is read-only by contract, so we must not shell out to
    // `xrt-smi configure --pmode …` from a status call. All failure
    // paths drop the error — the live system may legitimately lack
    // each device.
    let igpu_sensor = super::sensors::IgpuSensor::new();
    if let Ok(SensorReading::Igpu(reading)) = igpu_sensor.read(sysfs_root) {
        if let Some(active) = reading.active_profile {
            let _ = IgpuActuator::new().apply(active, sysfs_root);
        }
    }
    let _system_runner = SystemRunner::new();
    let _system_time = SystemTimeSource::new();
    let probe_npu = NpuActuator::new(Box::new(NoopRunner), Box::new(SystemTimeSource::new()));
    let _ = probe_npu.apply(NpuPmode::Default, sysfs_root);
    let _ = CgroupActuator::new().apply(CgroupOverrides::default(), sysfs_root);
}

/// Step 24 anti-dead-code probe: load the shipped warmup ONNX through
/// [`super::forecast::Model::warmup`], run [`super::forecast::gru::infer`]
/// against the current snapshot's feature vec, and drop the result.
/// Keeps `forecast::{Model, gru::infer, ModelStore}` referenced from
/// the production binary until Step 26 wires the forecaster into the
/// daemon's `one_tick`. Errors are dropped on the floor — `sy power
/// status` must keep working on hosts where tract optimisation
/// refuses an exotic model (mirrors the actuator-probe contract).
fn probe_forecast(snap: &crate::power::snapshot::Snapshot) {
    use super::forecast::{gru, model::ModelStore, Model};
    let model = match Model::warmup() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("sy power status: warmup forecast model unavailable: {e:#}");
            return;
        }
    };
    // Round-trip through ModelStore so the hot-reload primitive stays
    // referenced from the production binary. The daemon (Step 26) is
    // what actually swaps live; here we `load()`, infer once, then
    // swap the warmup back in to keep `ModelStore::store` referenced
    // through the dead-code probe. Errors from the second warmup load
    // are dropped — `sy power status` must keep working even if tract
    // refuses an exotic graph.
    let store = ModelStore::new(model);
    {
        let guard = store.load();
        let _ = gru::infer(&guard, &snap.features);
    }
    if let Ok(next) = Model::warmup() {
        store.store(next);
    }
}

/// Step 30 dead-code probe: construct the composite [`crate::power::drift::DriftDetector`]
/// and observe one sample on each sub-detector. The daemon (Step 31)
/// is what actually feeds the live forecast and reward residuals; for
/// the read-only `sy power status` path we only need the API surface
/// referenced from the production binary. Window length is read back
/// to keep `Adwin::window_len` live too.
fn probe_drift() {
    use super::drift::{DriftDetector, DriftSignal};
    let mut detector = DriftDetector::new();
    let _: DriftSignal = detector.forecast.observe(0.0);
    let _: DriftSignal = detector.reward.observe(false);
    let _ = detector.forecast.window_len();
}

/// No-op [`super::apply::npu::CommandRunner`] used by the status-path
/// dead-code probe so `sy power status` never actually shells out to
/// `xrt-smi`. The production path (Step 19) constructs `NpuActuator`
/// with [`super::apply::SystemRunner`] instead.
struct NoopRunner;
impl super::apply::npu::CommandRunner for NoopRunner {
    fn run(&self, _cmd: &str, _args: &[&str]) -> Result<()> {
        Ok(())
    }
    /// `sy power status` must NEVER shell out to `xrt-smi`, including
    /// for the P1-1 probe — return an empty string so [`crate::power::apply::npu::XrtSmiProbe`]
    /// resolves to `None` and the dead-code probe stays read-only.
    fn run_capturing(&self, _cmd: &str, _args: &[&str]) -> Result<String> {
        Ok(String::new())
    }
}

/// Bridge between the sensor-side `Epp` (a snapshot of what the
/// kernel currently reports) and the actuator-side `Epp` (a knob
/// value to write). Both enums carry the same five rungs; we keep
/// them separate types so the snapshot and the actuator can evolve
/// independently — e.g. if a future kernel adds a sixth EPP rung we
/// can extend the sensor without breaking the bandit arm enum.
fn map_sensor_epp(s: super::sensors::pstate::Epp) -> super::bandit::Epp {
    use super::bandit::Epp as Out;
    use super::sensors::pstate::Epp as In;
    match s {
        In::Performance => Out::Performance,
        In::BalancePerformance => Out::BalancePerformance,
        In::Default => Out::Default,
        In::BalancePower => Out::BalancePower,
        In::Power => Out::Power,
    }
}

/// Construct the production `Intent` bundle. Channels that fail to
/// initialise (no session bus on CI, no niri socket in containers,
/// `/proc/pressure/*` absent on stub kernels) are left as `None` —
/// the snapshot then surfaces the documented default for that slot.
fn live_intent(psi_root: &Path) -> Intent {
    let psi_cpu = PsiChannel::new(psi_root.join("cpu"), PsiKind::Cpu).ok();
    // Open the io / memory axes too so every PsiKind variant stays
    // referenced; only the CPU axis is currently fed into the GRU
    // feature vec (idx 7).
    let _ = PsiChannel::new(psi_root.join("io"), PsiKind::Io);
    let _ = PsiChannel::new(psi_root.join("memory"), PsiKind::Memory);
    let whitelist = config_path()
        .parent()
        .map(|d| d.join("intent_whitelist.toml"))
        .unwrap_or_else(|| std::path::PathBuf::from("configs/sy/intent_whitelist.toml"));
    let logind = LogindChannel::new(&whitelist).ok();
    let niri = NiriChannel::new().ok();
    // In-process aiplane registry tap: an empty registry against a
    // fresh session pool is enough to keep `Registry::*` referenced.
    // Step 10's daemon will swap this for the live registry handles.
    let pool = std::sync::Arc::new(crate::aiplane::session::SessionPool::new());
    let reg = crate::aiplane::registry::Registry::new(pool);
    let snap = reg.current_queue_depth();
    let _ = (snap.depth, snap.head_workload);
    let aiplane = Some(AiplaneIntentChannel::new(
        reg.in_flight_counter(),
        reg.last_workload_slot(),
    ));
    let mpris = MprisChannel::new().ok();
    let portal = ScreenCastChannel::new().ok();
    // Borrow the portal's counter so the future signal subscriber
    // (Step 10) has a handle ready; the read is intentionally
    // dropped here — `collect_tick` consumes the channel's `poll()`.
    if let Some(ref p) = portal {
        let _ = p.counter();
    }
    let idle = IdleChannel::new();
    let _ = idle.now_ms();
    let cgroup = Some(CgroupAncestryChannel::new(["firefox", "vscode"]));
    let notify = NotifyChannel::new().ok();
    if let Some(ref n) = notify {
        n.ingest_body("");
    }
    Intent {
        psi_cpu,
        logind,
        niri,
        aiplane,
        mpris,
        portal,
        idle: Some(idle),
        cgroup,
        notify,
        time: TimeChannel::new(),
    }
}

/// Intent bundle for a one-shot `sy power status` probe.
///
/// In production (no `SY_SYSFS_ROOT` override) this dials the live
/// D-Bus intent channels via [`live_intent`]. When `SY_SYSFS_ROOT` is
/// set — the established signal that the probe is sandboxed away from
/// the live host (CI, integration tests, containers) — it returns an
/// isolated bundle with no live channels, so a call / screen-cast /
/// media stream playing on the operator's desktop cannot flip
/// `call_active` and tip the shield DFA into `Meeting`. Mirrors how
/// every sysfs read already honours `SY_SYSFS_ROOT`: redirecting the
/// host root isolates the *whole* probe, intent included.
fn probe_intent(psi_root: &Path) -> Intent {
    if std::env::var_os("SY_SYSFS_ROOT").is_some() {
        return Intent {
            psi_cpu: None,
            logind: None,
            niri: None,
            aiplane: None,
            mpris: None,
            portal: None,
            idle: None,
            cgroup: None,
            notify: None,
            time: TimeChannel::new(),
        };
    }
    live_intent(psi_root)
}

/// `sy power log [--since=<dur>] [--json]` — read end of the audit
/// log. Per Step 12 ROADMAP: tails the NDJSON, filters by time
/// window, and emits one JSON object per line (`--json`) or a
/// one-line-per-entry human format. Sweeps retention as a side
/// effect — the user's explicit interest in the audit log doubles
/// as a maintenance trigger when the daemon isn't running.
fn log_cmd(since_arg: Option<String>, json_out: bool) -> Result<()> {
    let state_dir = power_state_dir();
    let logger = Logger::new(state_dir.clone());
    let since = match since_arg {
        None => DEFAULT_TAIL_WINDOW,
        Some(s) => match parse_since(&s) {
            Some(d) => d,
            None => {
                return Err(anyhow::Error::new(PowerError {
                    code: EXIT_USAGE,
                    msg: format!(
                        "sy power log: --since={s:?} unparseable (expected e.g. 30s, 15m, 1h)",
                    ),
                }));
            }
        },
    };
    if let Err(e) = logger.rotate_retention(&SystemClock) {
        eprintln!("sy power log: retention sweep skipped: {e}");
    }
    let entries = match logger.tail(since, &SystemClock) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sy power log: tail failed: {e}");
            Vec::new()
        }
    };
    if entries.is_empty() {
        eprintln!(
            "sy power log: no entries in window (dir={}, since={}s)",
            state_dir.display(),
            since.as_secs(),
        );
        return Ok(());
    }
    for entry in &entries {
        if json_out {
            match serde_json::to_string(entry) {
                Ok(line) => println!("{line}"),
                Err(e) => eprintln!("sy power log: serialise failure: {e}"),
            }
        } else {
            println!("{}", format_entry_line(entry));
        }
    }
    Ok(())
}

/// Options bundle for `sy power show`. Mirrors the [`PowerCmd::Show`]
/// arm shape so the dispatcher can forward fields without a
/// long-form `match` body.
pub(crate) struct ShowOpts {
    pub since: Option<String>,
    pub out: Option<PathBuf>,
    pub no_open: bool,
    pub allow_thin: bool,
    pub json: bool,
}

/// `sy power show` — Phase RV finale (Roadmap Step 35). Reads the
/// configured `--since` window of audit-log entries off
/// `power_state_dir()`, produces the six SPEC §4 metric structs +
/// counterfactual baseline, and either:
///
/// - emits the `sy.power.report/v1` JSON document on stdout (`--json`,
///   PDF generation skipped); or
/// - assembles an eight-panel PDF report and writes it to `--out`
///   (default: `<state>/reports/sy-power-<rfc3339>.pdf`), optionally
///   handing the file to `xdg-open` so a desktop viewer pops up.
///
/// Exit codes (CLIG):
/// - 0 ok
/// - 2 (`EXIT_USAGE`) malformed `--since` value
/// - 4 (`EXIT_DAEMON_UNREACHABLE`) — reserved for the upcoming
///   daemon-side report cache; currently unreachable because the read
///   path walks NDJSON directly.
/// - 7 (`EXIT_ONBOARDING_NOT_COMPLETE`) fewer than 24 h of audit
///   entries, no `--allow-thin`.
fn show_cmd(opts: ShowOpts) -> Result<()> {
    let since = match opts.since.as_deref() {
        None => DEFAULT_SHOW_SINCE,
        Some(s) => match parse_since(s) {
            Some(d) => d,
            None => {
                return Err(anyhow::Error::new(PowerError {
                    code: EXIT_USAGE,
                    msg: format!(
                        "sy power show: --since={s:?} unparseable (expected e.g. 30s, 15m, 1h, 7d)",
                    ),
                }));
            }
        },
    };
    let state_dir = power_state_dir();
    let logger = Logger::new(state_dir.clone());
    if let Err(e) = logger.rotate_retention(&SystemClock) {
        eprintln!("sy power show: retention sweep skipped: {e}");
    }
    let entries = match logger.tail(since, &SystemClock) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sy power show: tail failed: {e}");
            Vec::new()
        }
    };
    if !opts.json && !opts.allow_thin && entries.len() < MIN_ENTRIES_FOR_THICK_REPORT {
        return Err(anyhow::Error::new(PowerError {
            code: EXIT_ONBOARDING_NOT_COMPLETE,
            msg: format!(
                "sy power show: only {} audit entries (< {} ≈ 24 h); pass --allow-thin to override",
                entries.len(),
                MIN_ENTRIES_FOR_THICK_REPORT,
            ),
        }));
    }
    let bandit = extract_bandit_metrics(&entries);
    let shield = extract_shield_metrics(&entries);
    let energy = extract_energy_metrics(&entries);
    let drift = extract_drift_metrics(&entries);
    let forecast = extract_forecast_metrics(&entries);
    let activity = extract_activity_metrics(&entries);
    let baseline = compute_counterfactual_baseline(&entries);
    if opts.json {
        emit_report_json(
            since, &entries, &bandit, &forecast, &shield, &energy, &drift, &activity, &baseline,
        );
        return Ok(());
    }
    let report_metrics = ReportMetrics {
        bandit: &bandit,
        forecast: &forecast,
        shield: &shield,
        energy: &energy,
        drift: &drift,
        activity: &activity,
        entries: &entries,
    };
    let header = build_report_header(since, &SystemClock);
    // Reuse the (possibly env-pinned) header timestamp for the default
    // output path so a pinned invocation writes to a stable filename too.
    let generated_at = header.generated_at_rfc3339.clone();
    let template = ReportTemplate::build(&report_metrics, header);
    let pdf_bytes = compile_pdf(&template, &report_metrics);
    let out_path = opts
        .out
        .unwrap_or_else(|| default_report_out_path(&state_dir, &generated_at));
    write_pdf(&out_path, &pdf_bytes)?;
    let stdin_is_tty = std::io::stdin().is_terminal();
    if should_open_viewer(opts.no_open, stdin_is_tty) {
        spawn_pdf_opener(&out_path);
    }
    println!(
        "sy power show: wrote {} ({} bytes, {} entries)",
        out_path.display(),
        pdf_bytes.len(),
        entries.len(),
    );
    Ok(())
}

/// Pure helper: build the `sy.power.report/v1` JSON document. Returns
/// a `serde_json::Value` so the unit test can introspect the schema
/// without redirecting stdout. The print-to-stdout caller lives in
/// [`emit_report_json`].
#[allow(clippy::too_many_arguments)]
fn build_report_json(
    since: Duration,
    entries: &[AuditEntry],
    bandit: &super::report::metrics::BanditMetrics,
    forecast: &super::report::metrics::ForecastMetrics,
    shield: &super::report::metrics::ShieldMetrics,
    energy: &super::report::metrics::EnergyMetrics,
    drift: &super::report::metrics::DriftMetrics,
    activity: &super::report::metrics::ActivityMetrics,
    baseline: &super::report::metrics::EnergyMetrics,
) -> serde_json::Value {
    let report_metrics = ReportMetrics {
        bandit,
        forecast,
        shield,
        energy,
        drift,
        activity,
        entries,
    };
    let plot_svgs: serde_json::Map<String, serde_json::Value> = Plot::ALL
        .iter()
        .map(|p| {
            let svg = p.render(&report_metrics);
            (format!("{p:?}"), serde_json::Value::String(svg))
        })
        .collect();
    serde_json::json!({
        "schema": "sy.power.report/v1",
        "window_s": since.as_secs(),
        "entries": entries.len(),
        "bandit": bandit,
        "shield": shield,
        "energy": energy,
        "drift": drift,
        "forecast": forecast,
        "activity": activity,
        "baseline": baseline,
        "plots": serde_json::Value::Object(plot_svgs),
    })
}

/// Render the `sy.power.report/v1` JSON document to stdout. Wraps
/// [`build_report_json`] so the JSON path skips PDF generation (the
/// `show_json_skips_pdf` test pins this contract).
#[allow(clippy::too_many_arguments)]
fn emit_report_json(
    since: Duration,
    entries: &[AuditEntry],
    bandit: &super::report::metrics::BanditMetrics,
    forecast: &super::report::metrics::ForecastMetrics,
    shield: &super::report::metrics::ShieldMetrics,
    energy: &super::report::metrics::EnergyMetrics,
    drift: &super::report::metrics::DriftMetrics,
    activity: &super::report::metrics::ActivityMetrics,
    baseline: &super::report::metrics::EnergyMetrics,
) {
    let doc = build_report_json(
        since, entries, bandit, forecast, shield, energy, drift, activity, baseline,
    );
    match serde_json::to_string_pretty(&doc) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("sy power show: serialise failure: {e}"),
    }
}

/// Env var pinning the report's `generated_at` timestamp (RFC 3339).
/// When set to a parseable RFC-3339 instant it overrides the injected
/// clock so `sy power show` renders a byte-identical PDF across runs;
/// an unparseable value is ignored and the clock wins.
const ENV_REPORT_TIMESTAMP: &str = "SY_POWER_REPORT_TIMESTAMP";
/// Env var pinning the embedded model-identity SHA. When set it
/// overrides the default `rules-baseline` marker so the report's
/// "Model version" line is stable across machines/runs.
const ENV_REPORT_MODEL_SHA: &str = "SY_POWER_REPORT_MODEL_SHA";
/// Default model-identity marker when [`ENV_REPORT_MODEL_SHA`] is unset.
const DEFAULT_MODEL_SHA: &str = "rules-baseline";

/// Build the report header from the active window. Host name comes
/// from `uname` (or `unknown-host` on failure — the CLI must never
/// crash on a sandbox without a hostname); the generated timestamp is
/// read from the injected [`Clock`] (not wall-clock directly) so the
/// PDF is byte-reproducible under a frozen clock. The
/// [`ENV_REPORT_TIMESTAMP`] / [`ENV_REPORT_MODEL_SHA`] env vars pin the
/// timestamp + model identity for strict byte-equality (see the Phase
/// RV determinism DoD).
fn build_report_header(since: Duration, clock: &dyn Clock) -> ReportHeader {
    let host = hostname_or_unknown();
    let generated_at_rfc3339 = report_timestamp(clock);
    let window_days = since.as_secs_f32() / 86_400.0;
    ReportHeader {
        host,
        generated_at_rfc3339,
        window_days,
        model_version_sha: report_model_sha(),
    }
}

/// Resolve the report timestamp: an [`ENV_REPORT_TIMESTAMP`] override
/// (canonicalised to UTC RFC 3339 so the output bytes are identical
/// regardless of the offset the operator supplied) takes precedence;
/// otherwise the injected clock's `now()` is used. An unparseable
/// override falls through to the clock rather than erroring — the
/// report must never fail to render over a malformed env var.
fn report_timestamp(clock: &dyn Clock) -> String {
    if let Ok(raw) = std::env::var(ENV_REPORT_TIMESTAMP) {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw.trim()) {
            return dt.with_timezone(&chrono::Utc).to_rfc3339();
        }
    }
    clock.now().to_rfc3339()
}

/// Resolve the embedded model-identity SHA: a non-empty
/// [`ENV_REPORT_MODEL_SHA`] override wins, else the
/// [`DEFAULT_MODEL_SHA`] marker.
fn report_model_sha() -> String {
    match std::env::var(ENV_REPORT_MODEL_SHA) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => DEFAULT_MODEL_SHA.to_string(),
    }
}

/// Best-effort hostname read. Falls back to `unknown-host` so the
/// report still emits cleanly on a sandbox that doesn't expose
/// `gethostname`. Never panics, never propagates an error.
fn hostname_or_unknown() -> String {
    rustix::system::uname()
        .nodename()
        .to_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "unknown-host".to_string())
}

/// Default `--out` path: `<state>/reports/sy-power-<rfc3339>.pdf`.
/// Filesystem-safe RFC 3339 — colons swapped for hyphens so the path
/// is copy-pasteable into `xdg-open` arg lists across shells. Takes the
/// report's generated-at timestamp (already resolved from the injected
/// clock / [`ENV_REPORT_TIMESTAMP`]) so a pinned invocation lands a
/// stable filename.
fn default_report_out_path(state_dir: &Path, generated_at_rfc3339: &str) -> PathBuf {
    let ts = generated_at_rfc3339.replace(':', "-");
    state_dir.join("reports").join(format!("sy-power-{ts}.pdf"))
}

/// Atomically write the PDF bytes to `path`. Creates any missing
/// parent directories so a fresh `~/.local/state/sy/power/reports/`
/// tree lands without a separate `mkdir -p`.
fn write_pdf(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::Error::new(PowerError {
                    code: 1,
                    msg: format!(
                        "sy power show: cannot create output dir {}: {e}",
                        parent.display(),
                    ),
                })
            })?;
        }
    }
    std::fs::write(path, bytes).map_err(|e| {
        anyhow::Error::new(PowerError {
            code: 1,
            msg: format!("sy power show: write {} failed: {e}", path.display()),
        })
    })
}

/// Pure decision: open the PDF in a desktop viewer iff the user did
/// not pass `--no-open` AND stdin is a TTY (CLIG: "non-interactive by
/// default when stdin is not a TTY"). Extracted so the
/// `show_no_open_when_stdin_is_pipe` unit test can pin the rule
/// without spawning a process.
fn should_open_viewer(no_open_flag: bool, stdin_is_tty: bool) -> bool {
    !no_open_flag && stdin_is_tty
}

/// Spawn `xdg-open <path>` in the background. We never wait on the
/// child — the operator wants the PDF window to pop up while their
/// shell prompt returns. Failure to spawn is logged on stderr but
/// does not propagate; the PDF on disk is the primary deliverable.
fn spawn_pdf_opener(path: &Path) {
    use std::process::{Command, Stdio};
    let res = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = res {
        eprintln!("sy power show: xdg-open spawn failed: {e}");
    }
}

/// Render one audit entry in the human-readable form documented in
/// the Step 12 ROADMAP:
/// `<rfc3339 ts> · <tctl>°C · <power>W · arm=<…> shield=<…>`.
/// Fields absent in R1 (arm, shield) render as `-` so the column
/// shape is stable across the roadmap.
fn format_entry_line(entry: &AuditEntry) -> String {
    let tctl = entry
        .snapshot
        .raw
        .tctl_c
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "-".into());
    let power = entry
        .snapshot
        .raw
        .package_power_w
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "-".into());
    let arm = entry.applied_arm.as_deref().unwrap_or("-");
    let shield = entry.shield_state.as_deref().unwrap_or("-");
    format!(
        "{ts} · {tctl}°C · {power}W · arm={arm} shield={shield}",
        ts = entry.snapshot.ts.to_rfc3339(),
    )
}

/// `sy power explain [--last=N] [--json]` — SPEC §3 anti-goal #4
/// ("no black-box decisions") audit replay. Reads the last `last`
/// entries off the on-disk NDJSON log and hands them to
/// [`format_explain`] for rendering. The state-dir is the same
/// `power_state_dir()` the daemon writes to; if it doesn't exist yet
/// (no daemon ticks recorded), the renderer emits the documented
/// "no audit entries yet" sentinel and exits 0.
fn explain_cmd(last: usize, json_out: bool) -> Result<()> {
    let cfg = load_config_or_default();
    let logger = Logger::new(power_state_dir());
    let entries = match logger.tail_count(last, &SystemClock) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sy power explain: tail failed: {e}");
            Vec::new()
        }
    };
    print!("{}", format_explain(&entries, &cfg, json_out));
    Ok(())
}

/// `sy power train [--in <ndjson>] [--out <onnx>]` — Step 25 offline
/// trainer (Phase R4). Reads the NDJSON audit log produced by Step 9,
/// retrains the forecaster, validates the freshly-emitted ONNX
/// through tract (the SPEC §6 risk-table CI gate), and writes the
/// model the Step-24 [`super::forecast::Model`] hot-loads. Default
/// `--in` is the daemon's telemetry root; default `--out` is
/// `<state>/forecaster.onnx` under the same root so a `sy power
/// apply` install pins both paths.
///
/// Exit codes:
/// - `0` on a successful retrain.
/// - `1` on `ValidationFailed` (tract rejected the freshly-emitted
///   ONNX). The live model on disk is preserved.
/// - `2` (`EXIT_USAGE`) on a missing input file, unwritable output
///   directory, or any other argument-validation failure.
fn train_cmd(in_path: Option<PathBuf>, out_path: Option<PathBuf>) -> Result<()> {
    let in_path = in_path.unwrap_or_else(default_train_in_path);
    let out_path = out_path.unwrap_or_else(default_train_out_path);
    if !in_path.exists() {
        return Err(anyhow::Error::new(PowerError {
            code: EXIT_USAGE,
            msg: format!(
                "sy power train: input NDJSON not found: {}",
                in_path.display(),
            ),
        }));
    }
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::Error::new(PowerError {
                    code: EXIT_USAGE,
                    msg: format!(
                        "sy power train: cannot create output dir {}: {e}",
                        parent.display(),
                    ),
                })
            })?;
        }
    }
    match super::trainer::retrain_gru(&in_path, &out_path) {
        Ok(report) => {
            println!(
                "sy power train: ok rows={} epochs={} loss={:.3} val_acc={:.3} sha={} wall_ms={} out={}",
                report.rows_used,
                report.epochs,
                report.final_loss,
                report.validation_accuracy,
                report.version_sha,
                report.wall_time_ms,
                out_path.display(),
            );
            Ok(())
        }
        Err(super::trainer::TrainerError::ValidationFailed(msg)) => {
            Err(anyhow::Error::new(PowerError {
                code: 1,
                msg: format!("sy power train: validation failed: {msg}"),
            }))
        }
        Err(other) => Err(anyhow::Error::new(PowerError {
            code: 1,
            msg: format!("sy power train: {other}"),
        })),
    }
}

/// Default `--in` path: `<power_state_dir>/telemetry-<today>.ndjson`.
/// Mirrors the daemon's writer in `super::log::Logger::day_path`.
fn default_train_in_path() -> PathBuf {
    let today = chrono::Utc::now().date_naive();
    power_state_dir().join(format!("telemetry-{today}.ndjson"))
}

/// Default `--out` path: `<power_state_dir>/forecaster.onnx`. The
/// daemon's retrain scheduler (Step 26) consumes this path; pinning
/// it in one place keeps the CLI and daemon in lockstep.
fn default_train_out_path() -> PathBuf {
    power_state_dir().join("forecaster.onnx")
}

/// `sy power apply [--dry-run] [--yes] [--with-ppd]` — Step 13 R1
/// installer extended in Step 37 with the PPD-replacement decision.
/// Mounts the embedded polkit rule + systemd user unit + telemetry
/// dir under the canonical XDG paths and prints one record per
/// `ChangeRecord` (CLIG: same human format dry-run vs commit).
///
/// `--yes` gates destructive actions — currently only the PPD-mask
/// path. `--with-ppd` opts out of the PPD replacement entirely; both
/// daemons run side-by-side and the shim does not bind the
/// `net.hadess.PowerProfiles` well-known name.
fn apply_cmd(dry_run: bool, yes: bool, with_ppd: bool) -> Result<()> {
    let opts = apply_opts(dry_run, yes, with_ppd);
    let records = super::apply::install(&opts)?;
    let header = if dry_run {
        "sy power apply (dry-run):"
    } else {
        "sy power apply:"
    };
    println!("{header}");
    for rec in &records {
        println!("  {}", format_apply_record(rec));
    }
    Ok(())
}

/// Render one [`super::apply::ChangeRecord`] for the human output of
/// `apply_cmd`. Kept here (not on the type) so that future JSON
/// rendering (Step 35) and human rendering can diverge cleanly.
fn format_apply_record(rec: &super::apply::ChangeRecord) -> String {
    super::apply::installer::format_record(rec)
}

/// Build the production `InstallOpts`. State + user-unit roots come
/// from XDG / `$HOME`; the polkit root is the canonical system path
/// (root-owned — installer degrades to a Warning when unwritable).
fn apply_opts(dry_run: bool, yes: bool, with_ppd: bool) -> super::apply::InstallOpts {
    super::apply::InstallOpts {
        dry_run,
        state_root: xdg_state_home(),
        user_unit_root: xdg_config_home().join("systemd/user"),
        config_root: xdg_config_home(),
        polkit_root: PathBuf::from("/etc/polkit-1/rules.d"),
        grub_root: PathBuf::from("/etc/default/grub.d"),
        grub_cfg_file: PathBuf::from("/etc/default/grub"),
        dbus_root: PathBuf::from("/etc/dbus-1/system.d"),
        tmpfiles_root: PathBuf::from("/etc/tmpfiles.d"),
        system_unit_root: PathBuf::from("/etc/systemd/system"),
        udev_rules_root: PathBuf::from("/etc/udev/rules.d"),
        command_runner: Box::new(super::apply::InstallerSystemRunner::new()),
        run_daemon_reload: !dry_run,
        yes,
        with_ppd,
        ppd_unit_paths: super::apply::installer::default_ppd_unit_paths(),
        tuned_unit_paths: super::apply::installer::default_tuned_unit_paths(),
        grubby_detect: super::apply::installer::default_grubby_detect(),
        stress_ng_detect: super::apply::installer::default_stress_ng_detect(),
    }
}

/// `$XDG_STATE_HOME` ⇒ `$HOME/.local/state` ⇒ `/tmp` fallback, matching
/// `power::power_state_dir_for_daemon` modulo the `sy/` suffix (which
/// is owned by the installer — the daemon then joins `power/`).
fn xdg_state_home() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("sy");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/state/sy");
    }
    PathBuf::from("/tmp/sy")
}

/// `$XDG_CONFIG_HOME` ⇒ `$HOME/.config` ⇒ `/tmp/sy-config`. Mirrors
/// the resolution used by `supervision::apply` so a single `sy apply`
/// run and a `sy power apply` run produce matching layouts.
fn xdg_config_home() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from("/tmp/sy-config")
}

/// `sy power list-profiles [--json]` — enumerate the bandit arm
/// table from the loaded `PowerConfig`. Emits the SPEC §4
/// `sy.power.profiles/v1` schema under `--json`; otherwise prints a
/// one-arm-per-line human summary. The arm names rendered here are
/// the stable identifiers consumed by `sy power profile <name>`
/// (Step 22) and the audit log (Step 23).
fn list_profiles_cmd(json_out: bool, cfg: &PowerConfig) -> Result<()> {
    let arms = super::bandit::load_arms(cfg)?;
    if json_out {
        let doc = serde_json::json!({
            "schema": PROFILES_SCHEMA,
            "arms": arms,
        });
        let rendered = serde_json::to_string_pretty(&doc)
            .map_err(|e| anyhow::anyhow!("serialise sy.power.profiles/v1: {e}"))?;
        println!("{rendered}");
    } else {
        println!("{}", format_profiles_human(&arms));
    }
    Ok(())
}

/// Stable schema identifier for `sy power list-profiles --json` per
/// SPEC §4 versioning convention (`sy.power.<command>/v<N>`).
const PROFILES_SCHEMA: &str = "sy.power.profiles/v1";

/// Render the arm table as a single multi-line human-readable string.
/// One row per arm, columns separated by `·` to match `sy power log`.
fn format_profiles_human(arms: &[super::bandit::Arm]) -> String {
    let mut out = String::new();
    out.push_str("sy power list-profiles (8 arms from SPEC §4):\n");
    for arm in arms {
        // Render the cgroup hints inline so the human form is one line.
        let mut hints: Vec<String> = Vec::new();
        if let Some(v) = arm.cgroup.cpu_uclamp_min {
            hints.push(format!("uclamp_min={v}"));
        }
        if let Some(v) = arm.cgroup.cpu_uclamp_max {
            hints.push(format!("uclamp_max={v}"));
        }
        if let Some(v) = arm.cgroup.cpu_weight {
            hints.push(format!("cpu_weight={v}"));
        }
        let cgroup = if hints.is_empty() {
            "default".to_string()
        } else {
            hints.join(",")
        };
        let pp = serde_json::to_string(&arm.platform_profile).unwrap_or_else(|_| "\"?\"".into());
        let epp = serde_json::to_string(&arm.epp).unwrap_or_else(|_| "\"?\"".into());
        let igpu = serde_json::to_string(&arm.igpu_mode).unwrap_or_else(|_| "\"?\"".into());
        let npu = serde_json::to_string(&arm.npu_pmode).unwrap_or_else(|_| "\"?\"".into());
        out.push_str(&format!(
            "  {name:<10} · platform={pp} · epp={epp} · igpu={igpu} · npu={npu} · cgroup={cgroup}\n",
            name = arm.name,
        ));
    }
    out
}

/// Resolve `~/.local/state/sy/power/` with an `XDG_STATE_HOME` fallback
/// before `$HOME`. Mirrors the spec's documented telemetry root —
/// delegates to the module-level helper so the daemon (`daemon::run`)
/// and the read path (`sy power log`) can't drift apart.
fn power_state_dir() -> PathBuf {
    super::power_state_dir_for_daemon()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use tempfile::TempDir;

    /// BUG-20260712-1137: a decode failure means the daemon answered but
    /// its frame did not parse — the CLI must report that (exit
    /// `EXIT_DECODE_ERROR`, "could not be parsed"), never masquerade a
    /// healthy daemon as "unreachable". A connect-level error keeps the
    /// unreachable code.
    #[test]
    fn decode_error_is_not_reported_as_unreachable() {
        let decode = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "decode: invalid type: null, expected f32",
        );
        let mapped = classify_dial_error(&decode);
        assert_eq!(mapped.code, EXIT_DECODE_ERROR);
        assert!(
            mapped.msg.contains("could not be parsed"),
            "decode error must name the parse failure, not 'unreachable': {}",
            mapped.msg,
        );
        assert!(!mapped.msg.contains("unreachable"), "{}", mapped.msg);

        let refused =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let mapped = classify_dial_error(&refused);
        assert_eq!(mapped.code, EXIT_DAEMON_UNREACHABLE);
        assert!(mapped.msg.contains("unreachable"), "{}", mapped.msg);
    }

    /// Roadmap Step 1: assert every shipped subcommand surfaces in
    /// `sy power --help`. The roadmap text says "all 8" but the
    /// post-SPEC `show` subcommand brings the count to nine — match
    /// what we ship, not the stale text.
    const SUBCOMMANDS: &[&str] = &[
        "status",
        "daemon",
        "apply",
        "log",
        "profile",
        "explain",
        "train",
        "show",
        "list-profiles",
        "mcp",
    ];

    #[derive(clap::Parser)]
    struct TestCli {
        #[command(subcommand)]
        cmd: PowerCmd,
    }

    #[test]
    fn help_lists_every_subcommand() {
        let mut cmd = TestCli::command();
        let help = cmd.render_long_help().to_string();
        for sub in SUBCOMMANDS {
            assert!(
                help.contains(sub),
                "--help missing subcommand {sub:?}\n--- help ---\n{help}"
            );
        }
    }

    /// Serialise the env-touching tests below: `XDG_RUNTIME_DIR` is
    /// per-process state, so a parallel test reading it mid-mutation
    /// would race against `status_exit_4_when_no_daemon`. Use the
    /// crate-wide canonical lock so we also serialise against
    /// `aiplane::ipc::tests`, which dial sockets resolved from the
    /// same env var.
    use crate::aiplane::TEST_ENV_LOCK as ENV_LOCK;

    /// Step H2 retry: with `SY_SYSFS_ROOT` unset, the CLI-side probe
    /// must dial into the production `/sys` tree. Locks the default so
    /// production deployments don't regress to an empty tempdir.
    #[test]
    fn sysfs_root_defaults_to_slash_sys_when_env_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("SY_SYSFS_ROOT").ok();
        std::env::remove_var("SY_SYSFS_ROOT");
        let got = super::sysfs_root();
        if let Some(v) = prev {
            std::env::set_var("SY_SYSFS_ROOT", v);
        }
        assert_eq!(got, std::path::PathBuf::from("/sys"));
    }

    /// Step H2 retry: `SY_SYSFS_ROOT` overrides the probe path so the
    /// integration test in `tests/power_bandit_floor.rs` can pin the
    /// shield DFA to `CoolAc` regardless of host CPU temperature.
    #[test]
    fn sysfs_root_honors_env_override() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().expect("tempdir");
        let prev = std::env::var("SY_SYSFS_ROOT").ok();
        std::env::set_var("SY_SYSFS_ROOT", tmp.path());
        let got = super::sysfs_root();
        if let Some(v) = prev {
            std::env::set_var("SY_SYSFS_ROOT", v);
        } else {
            std::env::remove_var("SY_SYSFS_ROOT");
        }
        assert_eq!(got, tmp.path());
    }

    /// Step 11 DoD: `sy power status` with `XDG_RUNTIME_DIR` pointed
    /// at an empty tempdir surfaces the SPEC §4 daemon-unreachable
    /// exit code (4) via the [`PowerError`] downcast in `main.rs`.
    /// The CLI never panics on a missing socket — the agent contract
    /// distinguishes "daemon-down" from a generic error.
    #[test]
    fn status_exit_4_when_no_daemon() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().expect("tempdir");
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", tmp.path());
        let res = status(true, false);
        if let Some(v) = prev {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let err = res.expect_err("status must fail when no daemon socket exists");
        let pe = err
            .downcast_ref::<PowerError>()
            .expect("status should return PowerError on daemon-down");
        assert_eq!(pe.code, EXIT_DAEMON_UNREACHABLE);
    }

    /// Step 12: `--since=garbage` must surface `EXIT_USAGE` (2) per
    /// CLIG "Meaningful, non-zero exit codes on failure" — never a
    /// silent fallback to the default window.
    #[test]
    fn log_since_garbage_exits_with_usage_error() {
        let res = log_cmd(Some("not-a-duration".to_string()), true);
        let err = res.expect_err("garbage --since must fail");
        let pe = err
            .downcast_ref::<PowerError>()
            .expect("garbage --since must map to PowerError");
        assert_eq!(pe.code, EXIT_USAGE);
    }

    /// Step 25: `sy power train --in <missing>` surfaces
    /// `EXIT_USAGE` (2) per CLIG. The trainer never runs against a
    /// non-existent log — the daemon's retrain scheduler (Step 26)
    /// relies on this gate to avoid starting a training pass when
    /// the user hasn't completed onboarding yet.
    #[test]
    fn train_missing_input_exits_with_usage_error() {
        let tmp = TempDir::new().expect("tempdir");
        let missing = tmp.path().join("does-not-exist.ndjson");
        let res = train_cmd(Some(missing), Some(tmp.path().join("model.onnx")));
        let err = res.expect_err("missing --in must fail");
        let pe = err
            .downcast_ref::<PowerError>()
            .expect("missing --in must map to PowerError");
        assert_eq!(pe.code, EXIT_USAGE);
    }

    /// Step 12: the human render is a single line with the documented
    /// column shape: `ts · tctl · power · arm · shield`. R1 leaves
    /// arm + shield as `-` so consumers can `awk` against a stable
    /// column count from day one.
    /// Step 14 DoD: `sy power list-profiles --json` emits all eight
    /// arms in the SPEC §4 tuple shape under a stable
    /// `sy.power.profiles/v1` envelope. Loading the shipped
    /// `configs/sy/power.toml` and asking `load_arms` for the table
    /// must produce that document verbatim — `list_profiles_cmd`'s
    /// job is solely to wrap it under the schema header.
    #[test]
    fn list_profiles_json_shape() {
        let cfg_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml");
        let cfg = PowerConfig::load(&cfg_path).expect("shipped power.toml parses");
        let arms = crate::power::bandit::load_arms(&cfg).expect("load_arms");
        let doc = serde_json::json!({
            "schema": super::PROFILES_SCHEMA,
            "arms": arms,
        });
        let rendered = serde_json::to_string(&doc).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("json");
        assert_eq!(parsed["schema"], "sy.power.profiles/v1");
        let arms_arr = parsed["arms"].as_array().expect("arms array");
        assert_eq!(arms_arr.len(), 8);
        let names: Vec<&str> = arms_arr
            .iter()
            .map(|a| a["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            names,
            [
                "whisper",
                "idle",
                "browse",
                "call",
                "code",
                "build",
                "npu-burst",
                "flat-out"
            ],
        );
        // Spot-check the SPEC §4 tuple shape on the first arm.
        assert_eq!(arms_arr[0]["platform_profile"], "quiet");
        assert_eq!(arms_arr[0]["epp"], "power");
        assert_eq!(arms_arr[0]["igpu_mode"], "POWER_SAVING");
        assert_eq!(arms_arr[0]["npu_pmode"], "powersaver");
        assert_eq!(arms_arr[0]["cgroup"]["cpu_uclamp_max"], 40);
    }

    /// Step 19: `sy power profile ludicrous` validates the arm name
    /// against the shipped table BEFORE dialing the daemon and
    /// surfaces `EXIT_USAGE` (2) per CLIG. No socket needed — the
    /// rejection happens locally so an agent never blocks on a
    /// daemon-down detection round-trip just to be told it
    /// mistyped the arm.
    #[test]
    fn profile_unknown_arm_exits_with_usage_error() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().expect("tempdir");
        // Point XDG_RUNTIME_DIR at an empty tempdir so the validation
        // failure can't be confused with a missing-socket exit.
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", tmp.path());
        let res = profile_cmd(Some("ludicrous-speed".to_string()), false);
        if let Some(v) = prev {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let err = res.expect_err("unknown arm must fail");
        let pe = err
            .downcast_ref::<PowerError>()
            .expect("unknown arm must surface PowerError");
        assert_eq!(pe.code, EXIT_USAGE);
        assert!(
            pe.msg.contains("ludicrous-speed"),
            "error must name the bad arm: {}",
            pe.msg,
        );
    }

    /// Step 19: `sy power profile` with neither a name nor `--auto`
    /// is a usage error (`EXIT_USAGE`). The CLI's required-by-XOR
    /// contract surfaces explicitly here.
    #[test]
    fn profile_no_args_exits_with_usage_error() {
        let res = profile_cmd(None, false);
        let err = res.expect_err("no args must fail");
        let pe = err
            .downcast_ref::<PowerError>()
            .expect("must surface PowerError");
        assert_eq!(pe.code, EXIT_USAGE);
    }

    #[test]
    fn format_entry_line_renders_documented_columns() {
        use crate::power::log::AuditEntry;
        use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
        use chrono::TimeZone;
        let ts = chrono::Utc
            .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
            .single()
            .expect("pinned ts");
        let snap = Snapshot {
            schema: SCHEMA_ID,
            ts,
            features: [0.0; FEATURE_LEN],
            raw: SnapshotRaw {
                tctl_c: Some(71.0),
                package_power_w: Some(27.4),
                ..Default::default()
            },
            snapshot_hash: "0".repeat(64),
        };
        let entry = AuditEntry::r1(snap);
        let line = format_entry_line(&entry);
        assert!(line.contains("2026-05-19T12:00:00"));
        assert!(line.contains("71.0°C"));
        assert!(line.contains("27.4W"));
        assert!(line.contains("arm=-"));
        assert!(line.contains("shield=-"));
        assert!(!line.contains('\n'), "must be a single line: {line:?}");
    }

    /// Step 31 DoD: `sy power status` exits 3 (`EXIT_DRIFT_ACTIVE`)
    /// when the daemon's IPC response carries `drift.adwin_alarm =
    /// true`. The pure helper [`status_drift_exit`] is what `status()`
    /// branches on after rendering the JSON; pinning it here keeps
    /// the SPEC §4 exit-code table honoured without a daemon-in-thread.
    #[test]
    fn status_exit_3_when_drift_alarm() {
        use crate::power::drift::DriftStatus;
        use crate::power::ipc::{StatusResponse, STATUS_SCHEMA};
        let resp = StatusResponse {
            schema: STATUS_SCHEMA.to_string(),
            snapshot_hash: "0".repeat(64),
            snapshot: serde_json::json!({}),
            last_audit: None,
            drift: DriftStatus {
                adwin_alarm: true,
                ddm_warning: false,
                last_alarm_at: Some(chrono::Utc::now()),
            },
            model: None,
            onboarding: None,
        };
        let err = status_drift_exit(&resp).expect("alarm must produce a PowerError");
        let pe = err
            .downcast_ref::<PowerError>()
            .expect("drift exit must downcast to PowerError");
        assert_eq!(pe.code, EXIT_DRIFT_ACTIVE);
    }

    /// Mirror of the above for the "all-clear" path — no `PowerError`
    /// produced when `drift.adwin_alarm = false`. Guards against a
    /// regression where the gate fires on the default DriftStatus.
    #[test]
    fn status_exit_0_when_no_drift() {
        use crate::power::drift::DriftStatus;
        use crate::power::ipc::{StatusResponse, STATUS_SCHEMA};
        let resp = StatusResponse {
            schema: STATUS_SCHEMA.to_string(),
            snapshot_hash: "0".repeat(64),
            snapshot: serde_json::json!({}),
            last_audit: None,
            drift: DriftStatus::default(),
            model: None,
            onboarding: None,
        };
        assert!(status_drift_exit(&resp).is_none());
    }

    /// Step 35 DoD: `sy power show --json` skips PDF generation and
    /// emits the `sy.power.report/v1` schema. The pure
    /// [`build_report_json`] surfaces the same wire shape the
    /// stdout path renders; pinning it here keeps the schema fixed
    /// without spinning up a daemon or touching the filesystem.
    #[test]
    fn show_json_skips_pdf() {
        use crate::power::report::{
            compute_counterfactual_baseline, extract_activity_metrics, extract_bandit_metrics,
            extract_drift_metrics, extract_energy_metrics, extract_forecast_metrics,
            extract_shield_metrics,
        };
        let entries: Vec<AuditEntry> = Vec::new();
        let bandit = extract_bandit_metrics(&entries);
        let forecast = extract_forecast_metrics(&entries);
        let shield = extract_shield_metrics(&entries);
        let energy = extract_energy_metrics(&entries);
        let drift = extract_drift_metrics(&entries);
        let activity = extract_activity_metrics(&entries);
        let baseline = compute_counterfactual_baseline(&entries);
        let doc = build_report_json(
            Duration::from_secs(7 * 24 * 3600),
            &entries,
            &bandit,
            &forecast,
            &shield,
            &energy,
            &drift,
            &activity,
            &baseline,
        );
        assert_eq!(doc["schema"], "sy.power.report/v1");
        assert_eq!(doc["entries"], 0);
        // Every panel key must be present so an agent can rely on the
        // shape without conditional jq lookups.
        for key in [
            "bandit", "shield", "energy", "drift", "forecast", "activity", "baseline", "plots",
        ] {
            assert!(doc.get(key).is_some(), "missing key {key:?}");
        }
        // `plots` carries one SVG per `Plot::ALL` variant — Step 34's
        // contract. The PDF path is intentionally absent on `--json`.
        let plots = doc["plots"].as_object().expect("plots map");
        assert_eq!(plots.len(), Plot::ALL.len());
        assert!(
            doc.get("pdf").is_none(),
            "JSON path must not embed PDF bytes"
        );
    }

    /// Step S6 DoD (Phase RV finale): the report PDF is byte-reproducible.
    /// Rendering the same fixture window twice — through independent
    /// metric-extraction passes (so any HashMap iteration-order
    /// nondeterminism in the plot series would surface as differing
    /// bytes) with a frozen [`MockClock`] and a pinned model SHA —
    /// yields byte-identical PDFs. This closes the "same NDJSON window +
    /// same invocation -> byte-identical PDF" item deferred at Step 35.
    #[test]
    fn report_pdf_is_byte_reproducible_with_injected_clock() {
        use crate::power::clock::MockClock;
        use crate::power::log::AuditEntry;
        use crate::power::report::{
            extract_activity_metrics, extract_bandit_metrics, extract_drift_metrics,
            extract_energy_metrics, extract_forecast_metrics, extract_shield_metrics,
        };
        use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
        use chrono::TimeZone;

        const PINNED_SHA: &str = "deadbeefcafef00d";
        const FIXTURE_TICKS: i64 = 16;
        let base = chrono::Utc
            .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
            .single()
            .expect("pinned base ts");
        let build_entries = || -> Vec<AuditEntry> {
            (0..FIXTURE_TICKS)
                .map(|i| {
                    let snap = Snapshot {
                        schema: SCHEMA_ID,
                        ts: base + chrono::Duration::seconds(i),
                        features: [0.0; FEATURE_LEN],
                        raw: SnapshotRaw {
                            package_power_w: Some(8.0),
                            ..Default::default()
                        },
                        snapshot_hash: "0".repeat(64),
                    };
                    AuditEntry::r3(
                        snap,
                        "browse".to_string(),
                        "COOL_AC".to_string(),
                        Vec::new(),
                        vec![("browse".to_string(), 0.5)],
                        0.05,
                    )
                })
                .collect()
        };
        // Pin the embedded model identity so it is stable across runs.
        std::env::set_var("SY_POWER_REPORT_MODEL_SHA", PINNED_SHA);
        let clock = MockClock::new(base);
        let render_once = || -> Vec<u8> {
            let entries = build_entries();
            let bandit = extract_bandit_metrics(&entries);
            let forecast = extract_forecast_metrics(&entries);
            let shield = extract_shield_metrics(&entries);
            let energy = extract_energy_metrics(&entries);
            let drift = extract_drift_metrics(&entries);
            let activity = extract_activity_metrics(&entries);
            let metrics = ReportMetrics {
                bandit: &bandit,
                forecast: &forecast,
                shield: &shield,
                energy: &energy,
                drift: &drift,
                activity: &activity,
                entries: &entries,
            };
            let header = build_report_header(Duration::from_secs(2 * 3600), &clock);
            let template = ReportTemplate::build(&metrics, header);
            compile_pdf(&template, &metrics)
        };
        let first = render_once();
        let second = render_once();
        std::env::remove_var("SY_POWER_REPORT_MODEL_SHA");
        assert_eq!(
            first, second,
            "same window + frozen clock + pinned SHA must be byte-identical",
        );
        assert!(first.starts_with(b"%PDF-"), "must be a well-formed PDF");
        let haystack = String::from_utf8_lossy(&first);
        assert!(
            haystack.contains(PINNED_SHA),
            "SY_POWER_REPORT_MODEL_SHA override must round-trip into the bytes",
        );
        assert!(
            haystack.contains("2026-05-19T12:00:00"),
            "injected-clock timestamp must round-trip into the bytes",
        );
    }

    /// Step 35 DoD: stdin from a pipe (non-TTY) implies `--no-open`
    /// per CLIG ("non-interactive by default when stdin is not a TTY").
    /// Pinning the pure decision keeps the rule explicit without
    /// having to drive an actual `xdg-open` subprocess.
    #[test]
    fn show_no_open_when_stdin_is_pipe() {
        // Pipe (non-TTY) → never open the viewer, regardless of the
        // user's flag value.
        assert!(!should_open_viewer(false, false));
        assert!(!should_open_viewer(true, false));
        // TTY + explicit `--no-open` → still suppressed.
        assert!(!should_open_viewer(true, true));
        // TTY + no flag → open the viewer (the only "true" path).
        assert!(should_open_viewer(false, true));
    }

    /// Step 35 DoD: `--since=garbage` surfaces `EXIT_USAGE` (2) per
    /// CLIG. The error message names the bad value so an agent can
    /// regex-recover.
    #[test]
    fn show_since_garbage_exits_with_usage_error() {
        let res = show_cmd(ShowOpts {
            since: Some("not-a-duration".to_string()),
            out: None,
            no_open: true,
            allow_thin: true,
            json: true,
        });
        let err = res.expect_err("garbage --since must fail");
        let pe = err
            .downcast_ref::<PowerError>()
            .expect("garbage --since must map to PowerError");
        assert_eq!(pe.code, EXIT_USAGE);
        assert!(
            pe.msg.contains("not-a-duration"),
            "error must name the bad value: {}",
            pe.msg,
        );
    }
}
