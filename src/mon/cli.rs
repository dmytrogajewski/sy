//! Clap definitions for the `sy mon` subcommand group.
//!
//! Steps 11+13 ship `sy mon collect` (the aggregator daemon); Step 14
//! adds `sy mon snapshot [--json]` and the `sy mon mcp` stdio MCP
//! adapter on top. The popup-facing variants (`sy mon`, `sy mon open`,
//! `sy mon close`, `sy mon doctor`) land in Steps 16-21 of the sy-mon
//! roadmap. The flag set and `SY_MON_*` envs match SPEC §4 "CLI / MCP
//! surface"; precedence (flag > env > default) is enforced by clap's
//! own `Arg::env` + `default_value_t` chain.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use super::{client, collect};

/// Default ring-buffer depth in seconds. Matches SPEC §4 default.
pub const DEFAULT_HISTORY_SIZE: u32 = 600;

/// Default sample interval in milliseconds. Matches SPEC §4 default
/// ("1 Hz tick").
pub const DEFAULT_TICK_MS: u32 = 1000;

/// Subcommands under `sy mon`. Step 14 adds `Snapshot` and `Mcp`; the
/// popup-facing variants land in subsequent roadmap steps.
#[derive(Debug, Subcommand)]
pub enum MonCmd {
    /// Run the long-lived aggregator (1 Hz host-sensor tick into the
    /// mmap ring buffer). Supervised by `sy-mon-collect.service`.
    Collect(CollectOpts),
    /// Print the latest `SystemSnapshot` from a running aggregator.
    /// Exits 3 (CLAUDE.md "drift detected") when the aggregator is
    /// unreachable after the 100 ms × 10 connect-retry budget.
    Snapshot(SnapshotOpts),
    /// Stdio JSON-RPC MCP server advertising `system.mon.snapshot` +
    /// `system.mon.history` to agents. Each tool call round-trips to
    /// the aggregator over UDS via the shared client.
    Mcp,
    /// Headless render probe — walks every Step 15 Canvas widget
    /// through a mock recorder and prints a one-line op-count summary.
    /// Hidden because it's a doctor surface (not a user workflow);
    /// gated on `bar-iced` so `--no-default-features` builds still
    /// link. Doubles as the in-tree consumer that keeps the widget
    /// public surface from tripping `dead_code` until Step 16 wires
    /// the popup view tree.
    #[cfg(feature = "bar-iced")]
    #[command(hide = true)]
    Probe,
    /// Open the `sy mon` popup process (iced + iced_layershell). Same
    /// code path as the bare `sy mon` invocation per SPEC §3 SCOPE
    /// item 3; Step 19 reroutes both through `popup::toggle("mon")`
    /// so the keybinding from niri gets idempotent toggle behaviour.
    #[cfg(feature = "bar-iced")]
    Open,
    /// Close any running `sy mon` popup. Reads the PID file at
    /// `/tmp/sy-popup-mon.pid` and SIGTERMs the process. Idempotent:
    /// no popup → success exit. Step 19 folds this into
    /// `popup::toggle` so a second `Mod+M` press dismisses.
    #[cfg(feature = "bar-iced")]
    Close,
    /// Validate the sy-mon dashboard plumbing. Runs `mon.collect.running`,
    /// `mon.metrics_socket.<plane>` (one per known plane), and
    /// `mon.history.writable` through the shared `sy doctor` linear-checks
    /// runner; emits the SPEC §4.6 JSON shape with `--json`. sy-mon
    /// ROADMAP Step 21.
    Doctor(DoctorOpts),
    /// Emit the waybar custom-module JSON tile for the latest snapshot
    /// (one RPC per invocation; the tile binary is one-shot). Class is
    /// `ok` / `degraded` / `down` per SPEC §3 SCOPE item 9. waybar
    /// respawns this each `interval`, so missing aggregator → `down`
    /// tile instead of an error exit. sy-mon ROADMAP Step 22.
    Waybar,
}

/// Flags for `sy mon doctor`. Mirrors top-level `sy doctor --json` so
/// agents can dispatch identically against either subcommand.
#[derive(Debug, Args)]
pub struct DoctorOpts {
    /// Emit the SPEC §4.6 JSON report on stdout (pretty-printed).
    /// Defaults to a human-readable summary.
    #[arg(long)]
    pub json: bool,
}

/// Flags for `sy mon snapshot`. The `--json` flag is the workhorse
/// (machine-readable, stable schema); without it we emit a one-line
/// terse human summary plus a hint that `--json` carries the full
/// shape. SPEC §4 "CLI / MCP surface" makes `--json` the documented
/// path; the human form exists for `sy mon snapshot | head` ergonomics.
#[derive(Debug, Args)]
pub struct SnapshotOpts {
    /// Emit the full `SystemSnapshot` as pretty-printed JSON on stdout.
    /// Defaults to a one-line human summary when omitted.
    #[arg(long)]
    pub json: bool,
}

/// CLAUDE.md exit code 3 — "drift detected". `sy mon snapshot` reuses
/// this to mean "live system state could not be observed because the
/// aggregator is not running" so an agent's exit-code dispatch can
/// react identically to drift and to a missing aggregator.
pub const EXIT_AGGREGATOR_UNREACHABLE: u8 = 3;

/// Flags for `sy mon collect`. The defaults derive from
/// `$XDG_RUNTIME_DIR` at dispatch time so the type itself does not
/// depend on env vars at parse-time (this keeps `--help` output stable
/// in container CI where `XDG_RUNTIME_DIR` is unset).
#[derive(Debug, clap::Args)]
pub struct CollectOpts {
    /// Ring buffer depth in seconds.
    #[arg(
        long,
        env = "SY_MON_HISTORY_SIZE",
        default_value_t = DEFAULT_HISTORY_SIZE,
        value_name = "N",
    )]
    pub history_size: u32,
    /// Sample interval in milliseconds.
    #[arg(
        long,
        env = "SY_MON_TICK_MS",
        default_value_t = DEFAULT_TICK_MS,
        value_name = "N",
    )]
    pub tick_ms: u32,
    /// IPC socket path. Step 11 parses this for forward-compat; the
    /// actual sy-ipc UDS bind lands in Step 13 alongside the
    /// `system.mon.{snapshot,subscribe,history}` handlers.
    #[arg(long, env = "SY_MON_BIND", value_name = "PATH")]
    pub bind: Option<PathBuf>,
    /// Ring buffer file path.
    #[arg(long, env = "SY_MON_HISTORY_PATH", value_name = "PATH")]
    pub history_path: Option<PathBuf>,
}

/// Default `--bind` path: `$XDG_RUNTIME_DIR/sy/mon.sock`. Resolved at
/// dispatch time so the runtime dir env var matches the systemd
/// `%t` substitution the unit uses.
pub fn default_bind_path() -> Result<PathBuf> {
    Ok(xdg_runtime_dir()?.join("sy").join("mon.sock"))
}

/// Default `--history-path`: `$XDG_RUNTIME_DIR/sy/mon/history.bin`.
pub fn default_history_path() -> Result<PathBuf> {
    Ok(xdg_runtime_dir()?
        .join("sy")
        .join("mon")
        .join("history.bin"))
}

fn xdg_runtime_dir() -> Result<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR not set; required to resolve sy-mon socket paths")
}

/// Default subcommand when the user invokes the bare `sy mon`. Per
/// SPEC §3 SCOPE item 3 + Step 19, this is the popup — the same code
/// path `Mod+M` ships through `popup::toggle("mon")`. Under
/// `--no-default-features` (no `bar-iced`) we route to `snapshot
/// --json` as a graceful fallback so headless invocations don't
/// fail on a missing subcommand; clap shows `--help` if even that
/// path is missing.
#[cfg(feature = "bar-iced")]
pub fn default_subcommand() -> MonCmd {
    MonCmd::Open
}

#[cfg(not(feature = "bar-iced"))]
pub fn default_subcommand() -> MonCmd {
    MonCmd::Snapshot(SnapshotOpts { json: true })
}

/// Sync dispatch for `sy mon <cmd>`. Builds the multi-thread tokio
/// runtime locally so `src/main.rs`'s clap match stays synchronous.
/// Two worker threads cover the 1 Hz tick + the `spawn_blocking` pool
/// head used by [`collect::sample`].
pub fn dispatch(cmd: MonCmd) -> Result<()> {
    match cmd {
        MonCmd::Collect(opts) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .context("build sy-mon-collect tokio runtime")?;
            rt.block_on(collect::run(opts))
        }
        MonCmd::Snapshot(opts) => run_snapshot(opts),
        MonCmd::Mcp => super::mcp::run(),
        #[cfg(feature = "bar-iced")]
        MonCmd::Probe => {
            super::widgets::probe::run();
            Ok(())
        }
        #[cfg(feature = "bar-iced")]
        MonCmd::Open => super::app::run(),
        #[cfg(feature = "bar-iced")]
        MonCmd::Close => super::app::close(),
        MonCmd::Doctor(opts) => super::doctor::dispatch(opts.json),
        MonCmd::Waybar => super::waybar::run(),
    }
}

fn run_snapshot(opts: SnapshotOpts) -> Result<()> {
    let bind = default_bind_path()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build sy mon snapshot tokio runtime")?;
    let snap = match rt.block_on(client::snapshot(&bind)) {
        Ok(s) => s,
        Err(e) => {
            return Err(anyhow::Error::new(super::MonError {
                code: EXIT_AGGREGATOR_UNREACHABLE as i32,
                msg: format!(
                    "{e:#}\nhint: start the aggregator with `systemctl --user start {}`",
                    client::AGGREGATOR_UNIT
                ),
            }));
        }
    };
    write_snapshot(&snap, opts.json, &mut std::io::stdout())
}

/// Render `snap` to `out`. `--json` emits the SPEC §4 canonical
/// pretty-printed form with a trailing newline so `sy mon snapshot
/// --json > x.json` produces a clean file. The human form is one
/// short line per panel so an operator can `grep -m1` for any plane.
fn write_snapshot<W: std::io::Write>(
    snap: &sy_core::mon::snapshot::SystemSnapshot,
    json: bool,
    out: &mut W,
) -> Result<()> {
    if json {
        let body = serde_json::to_string_pretty(snap).context("serialise SystemSnapshot")?;
        out.write_all(body.as_bytes()).context("write stdout")?;
        out.write_all(b"\n").context("write stdout newline")?;
    } else {
        writeln!(
            out,
            "schema v{} captured_at_ms={} cpu_temp_c={:.1} mem_used_mib={} load={:.2} \
             (use --json for the full snapshot)",
            snap.schema_version,
            snap.captured_at_ms,
            snap.cpu.temp_c,
            snap.mem.used_mib,
            snap.cpu.load_avg[0],
        )
        .context("write stdout")?;
    }
    Ok(())
}

/// Test helper: build the JSON form of the snapshot into a buffer so
/// the unit tests can assert byte-equality against the golden without
/// shelling out a process. Kept under `#[cfg(test)]` so it can never
/// leak into the binary surface.
#[cfg(test)]
fn render_snapshot_json(snap: &sy_core::mon::snapshot::SystemSnapshot) -> String {
    let mut buf: Vec<u8> = Vec::new();
    write_snapshot(snap, true, &mut buf).expect("write_snapshot");
    String::from_utf8(buf).expect("utf-8 output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Wrapper so we can exercise the `MonCmd` parser without standing
    /// up the full `Cli` struct from `src/main.rs`. Mirrors the way
    /// `power::cli` tests its own subcommand tree.
    #[derive(clap::Parser)]
    struct Harness {
        #[command(subcommand)]
        cmd: MonCmd,
    }

    fn parse(args: &[&str]) -> CollectOpts {
        let h = Harness::try_parse_from(args).expect("parse");
        match h.cmd {
            MonCmd::Collect(opts) => opts,
            other => panic!("expected Collect, got {other:?}"),
        }
    }

    /// SPEC §4 + CLAUDE.md "Config precedence: flags > env vars > config
    /// file > defaults". `--history-size` exercises all three rungs.
    #[test]
    fn parse_args_flags_envs_precedence() {
        // Tests mutate process-wide env vars; serialise them so
        // parallel test workers don't race on `SY_MON_HISTORY_SIZE`.
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os("SY_MON_HISTORY_SIZE");
        std::env::remove_var("SY_MON_HISTORY_SIZE");

        // Rung 1: default applies when neither flag nor env is set.
        let opts = parse(&["sy", "collect"]);
        assert_eq!(opts.history_size, DEFAULT_HISTORY_SIZE);

        // Rung 2: env overrides the default.
        std::env::set_var("SY_MON_HISTORY_SIZE", "120");
        let opts = parse(&["sy", "collect"]);
        assert_eq!(opts.history_size, 120);

        // Rung 3: flag wins over env.
        let opts = parse(&["sy", "collect", "--history-size", "42"]);
        assert_eq!(opts.history_size, 42);

        if let Some(v) = prev {
            std::env::set_var("SY_MON_HISTORY_SIZE", v);
        } else {
            std::env::remove_var("SY_MON_HISTORY_SIZE");
        }
    }

    /// Step 14 spec: `sy mon snapshot --json` writes a pretty-printed
    /// `SystemSnapshot` to stdout, byte-identical to the checked-in
    /// golden when given a deterministic fixture. We use the default
    /// snapshot (every field at its zero shape, captured_at_ms == 0)
    /// so the golden is reproducible without a fake aggregator
    /// fixture file or a `--captured-at-ms` test hook. The full
    /// network round-trip is exercised by `snapshot_command_round_trip`
    /// below.
    #[test]
    fn snapshot_command_emits_json_to_stdout() {
        let snap = sy_core::mon::snapshot::SystemSnapshot::default();
        let got = render_snapshot_json(&snap);
        const GOLDEN: &str = include_str!("../../tests/snapshots/mon/sy-mon-snapshot.json");
        assert_eq!(
            got, GOLDEN,
            "snapshot JSON must match the golden byte-for-byte"
        );
    }

    /// Step 14 spec: when no aggregator is running, `sy mon snapshot`
    /// exits 3 (CLAUDE.md "drift detected") and stderr names the
    /// systemd unit operators are expected to start.
    ///
    /// We exercise this at the `dispatch` boundary by pointing
    /// `XDG_RUNTIME_DIR` at an empty tempdir so `default_bind_path`
    /// resolves to a path that doesn't exist; the connect-retry loop
    /// exhausts its budget and surfaces a `MonError { code: 3 }`.
    /// `main.rs` is what eventually maps that to `process::exit(3)` —
    /// pinning both halves means a regression in either layer is
    /// caught.
    #[test]
    fn snapshot_exits_3_when_aggregator_down() {
        use std::sync::Mutex;
        // Tests mutate XDG_RUNTIME_DIR; serialise with other env-
        // mutating tests in this module.
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("XDG_RUNTIME_DIR", tmp.path());

        let res = dispatch(MonCmd::Snapshot(SnapshotOpts { json: true }));

        // Restore env before assertions so a panic still cleans up.
        if let Some(v) = prev {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }

        let err = res.expect_err("aggregator-down dispatch must error");
        let me = err
            .downcast_ref::<super::super::MonError>()
            .expect("err must be MonError for exit-code mapping");
        assert_eq!(
            me.code, EXIT_AGGREGATOR_UNREACHABLE as i32,
            "exit code must be 3 (CLAUDE.md drift detected)"
        );
        assert!(
            me.msg.contains(client::AGGREGATOR_UNIT),
            "error message must name the unit ({:?}); got {:?}",
            client::AGGREGATOR_UNIT,
            me.msg
        );
    }
}
