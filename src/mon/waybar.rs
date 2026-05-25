//! `sy mon waybar` — emits a one-shot waybar custom-module JSON tile
//! summarising the latest [`SystemSnapshot`].
//!
//! Source: `specs/research/sy-mon/SPEC.md` §3 SCOPE item 9 + §9; sy-mon
//! ROADMAP Step 22. The tile is a thin adapter over the Step 14
//! [`super::client::snapshot`] IPC call — one RPC per waybar tick — so
//! the same aggregator that backs `sy mon snapshot` and the popup
//! drives the tile. Class taxonomy mirrors the dashboard banner:
//!
//! * `ok` — aggregator reachable, no plane errors, every supervised
//!   plane reports `state == "active"` / `"running"`.
//! * `degraded` — aggregator reachable but at least one plane has an
//!   entry in [`SystemSnapshot::errors`] or a non-active supervisor
//!   state.
//! * `down` — aggregator unreachable (snapshot RPC fails).
//!
//! Tile rendering stays in this module as a pure function over
//! `Option<&SystemSnapshot>` (`None` ⇒ `down`). The CLI dispatch in
//! [`super::cli`] performs the IPC, maps any error to `None`, and
//! prints the rendered tile. Unit tests below exercise the pure path;
//! the network round-trip is already covered by Step 14's snapshot
//! tests.

use anyhow::{Context, Result};

use sy_core::mon::snapshot::SystemSnapshot;

use super::{cli, client};

/// CSS class on a fully-healthy tick.
const CLASS_OK: &str = "ok";
/// CSS class when the aggregator is up but at least one plane is in
/// trouble (error entries or non-`active` supervisor state).
const CLASS_DEGRADED: &str = "degraded";
/// CSS class when the aggregator is unreachable.
const CLASS_DOWN: &str = "down";

/// Nerd Font glyph rendered in the tile body — mdi-monitor-dashboard.
/// Matches the dashboard chrome's glyph language (cpu/npu tiles use
/// 󰍛 chip; mon uses the monitor-dashboard glyph so it reads as
/// "system overview" at a glance).
const GLYPH: &str = "\u{F133A}";

/// Render the waybar JSON tile for the given snapshot. `None` means
/// the aggregator snapshot RPC failed — the tile collapses to the
/// "down" class with a tooltip pointing operators at the unit name.
pub fn tile_from_snapshot(snap: Option<&SystemSnapshot>) -> String {
    let Some(snap) = snap else {
        let tooltip = format!(
            "sy-mon aggregator unreachable\\nstart with: systemctl --user start {}",
            client::AGGREGATOR_UNIT
        );
        return format!(r#"{{"text":"{GLYPH}","class":"{CLASS_DOWN}","tooltip":"{tooltip}"}}"#);
    };
    let class = classify(snap);
    let tooltip = tooltip_for(snap, class);
    format!(r#"{{"text":"{GLYPH}","class":"{class}","tooltip":"{tooltip}"}}"#)
}

/// Three-way classification of a live snapshot.
///
/// `errors[]` non-empty OR any supervisor plane in a non-active state
/// ⇒ `degraded`; otherwise `ok`. `down` is only reachable via the
/// `None` branch of [`tile_from_snapshot`] (snapshot RPC failure).
fn classify(snap: &SystemSnapshot) -> &'static str {
    if !snap.errors.is_empty() || snap.supervisor.planes.iter().any(|p| !is_healthy(&p.state)) {
        CLASS_DEGRADED
    } else {
        CLASS_OK
    }
}

/// Whether a supervisor plane state string counts as "healthy" for the
/// tile classifier. Mirrors the popup banner's accepting set so the
/// two surfaces never disagree about colour.
fn is_healthy(state: &str) -> bool {
    matches!(state, "active" | "running" | "Running" | "Active")
}

/// Tooltip body — multi-line via `\n` JSON escapes so waybar's
/// `tooltip: true` consumer renders the panel summary on hover.
fn tooltip_for(snap: &SystemSnapshot, class: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("sy mon — {class}"));
    lines.push(format!(
        "cpu {:.1}°C  mem {} MiB  load {:.2}",
        snap.cpu.temp_c, snap.mem.used_mib, snap.cpu.load_avg[0]
    ));
    if !snap.supervisor.planes.is_empty() {
        let summary: Vec<String> = snap
            .supervisor
            .planes
            .iter()
            .map(|p| format!("{}={}", p.name, p.state))
            .collect();
        lines.push(format!("planes: {}", summary.join(", ")));
    }
    if !snap.errors.is_empty() {
        // Truncate to the first few errors so the tooltip stays small.
        let head: Vec<String> = snap
            .errors
            .iter()
            .take(3)
            .map(|e| format!("{}:{}", e.plane, e.kind))
            .collect();
        lines.push(format!("errors: {}", head.join(", ")));
    }
    lines.push("click: open sy mon popup".to_string());
    lines.join("\\n")
}

/// Dispatch entry-point: fetch the snapshot from the aggregator over
/// UDS, render the tile, and print it on stdout (one line, no
/// trailing whitespace beyond the println). On RPC failure the tile
/// renders the `down` class — waybar gets a parseable line every
/// tick even when the aggregator is dead, so the tile collapses
/// gracefully rather than disappearing.
pub fn run() -> Result<()> {
    let bind = cli::default_bind_path()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build sy mon waybar tokio runtime")?;
    let snap = rt.block_on(client::snapshot(&bind)).ok();
    println!("{}", tile_from_snapshot(snap.as_ref()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use sy_core::mon::snapshot::{MonError, PlanePanel, SupervisorPanel, SystemSnapshot};

    /// Step 22 spec: tile JSON is byte-equal to a checked-in golden
    /// for a deterministic fixture. We use the default snapshot
    /// (every panel at zero, no errors, no planes) so the green
    /// branch is the canonical reference and the golden stays
    /// reproducible without a fake aggregator state file.
    #[test]
    fn tile_json_shape() {
        let snap = SystemSnapshot::default();
        let got = tile_from_snapshot(Some(&snap));
        const GOLDEN: &str = include_str!("../../tests/snapshots/waybar/mon-ok.json");
        assert_eq!(format!("{got}\n"), GOLDEN);
    }

    /// Step 22 spec: a snapshot with one plane in trouble flips the
    /// tile to `degraded`. We tag a synthetic `MonError` so the
    /// classifier's "any error ⇒ degraded" branch fires; the test
    /// would also pass with a non-active supervisor row but pinning
    /// the error path here keeps the assertion narrow.
    #[test]
    fn yellow_when_any_plane_degraded() {
        let snap = SystemSnapshot {
            errors: vec![MonError {
                plane: "knowledge".to_string(),
                kind: "timeout".to_string(),
                message: "scrape exceeded budget".to_string(),
            }],
            supervisor: SupervisorPanel {
                planes: vec![PlanePanel {
                    name: "knowledge".to_string(),
                    state: "active".to_string(),
                    restarts: 0,
                }],
            },
            ..SystemSnapshot::default()
        };
        let got = tile_from_snapshot(Some(&snap));
        assert!(
            got.contains(&format!(r#""class":"{CLASS_DEGRADED}""#)),
            "errors[] non-empty must flip class to {CLASS_DEGRADED}; got {got}"
        );
        // And the same path through the supervisor state branch.
        let snap2 = SystemSnapshot {
            supervisor: SupervisorPanel {
                planes: vec![PlanePanel {
                    name: "aiplane".to_string(),
                    state: "failed".to_string(),
                    restarts: 3,
                }],
            },
            ..SystemSnapshot::default()
        };
        let got2 = tile_from_snapshot(Some(&snap2));
        assert!(
            got2.contains(&format!(r#""class":"{CLASS_DEGRADED}""#)),
            "non-active supervisor state must flip class to {CLASS_DEGRADED}; got {got2}"
        );
    }

    /// Step 22 spec: a snapshot fetch failure (aggregator down,
    /// stale socket, …) renders the tile in `down` class. The
    /// CLI dispatch maps `client::snapshot` errors to `None`, so
    /// the pure function's `None` branch is the contract.
    #[test]
    fn red_when_aggregator_down() {
        let got = tile_from_snapshot(None);
        assert!(
            got.contains(&format!(r#""class":"{CLASS_DOWN}""#)),
            "aggregator-down branch must emit class {CLASS_DOWN}; got {got}"
        );
        // Tooltip must name the systemd unit so operators know what
        // to start. Mirrors the `sy mon snapshot` hint shape.
        assert!(
            got.contains(client::AGGREGATOR_UNIT),
            "down-tile tooltip must name {} for operator dispatch; got {got}",
            client::AGGREGATOR_UNIT
        );
    }
}
