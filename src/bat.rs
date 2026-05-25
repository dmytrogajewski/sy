//! Battery applet — emits waybar JSON with a Font Awesome battery glyph
//! that changes with charge level. Charging state is shown by prepending
//! a bolt glyph.
//!
//! Adapter over [`sy_core::sensors::bat::sample()`] (sy-mon ROADMAP
//! Step 5): the sysfs walk over `BAT*` directories and the field
//! parsing live in the shared sensor so `sy mon` and the waybar tile
//! share one read path per metric.

use anyhow::Result;

use sy_core::sensors::bat::{self, BatteryStatus};

// Font Awesome battery ramp (JetBrainsMono Nerd Font).
const BAT_EMPTY: &str = "\u{F244}"; // fa-battery-0
const BAT_QUARTER: &str = "\u{F243}"; // fa-battery-1
const BAT_HALF: &str = "\u{F242}"; // fa-battery-2
const BAT_THREE_Q: &str = "\u{F241}"; // fa-battery-3
const BAT_FULL: &str = "\u{F240}"; // fa-battery-4
const BOLT: &str = "\u{F0E7}"; // fa-bolt (charging)

pub fn run(waybar: bool) -> Result<()> {
    if waybar {
        println!("{}", waybar_out(read_first_battery()));
        Ok(())
    } else {
        // Future: a battery info popup. For now this is bar-only.
        Ok(())
    }
}

/// Pure formatter — takes the read result (or `None` for desktops with
/// no `BAT*` directory) and produces the tile JSON. Tested via a
/// golden in `tests/snapshots/waybar/bat-*.json`.
fn waybar_out(read: Option<(u8, String)>) -> String {
    let Some((cap, status)) = read else {
        // No battery (desktop) — emit empty so the tile collapses.
        return r#"{"text":"","class":"hidden","tooltip":""}"#.to_string();
    };
    let charging = status == "Charging";
    let critical = !charging && cap <= 15;
    let class = if charging {
        "charging"
    } else if critical {
        "critical"
    } else if cap <= 30 {
        "low"
    } else if cap <= 60 {
        "mid"
    } else if cap <= 99 {
        "high"
    } else {
        "full"
    };
    let body = bucket_glyph(cap);
    let text = if charging {
        format!("{BOLT}{body}")
    } else {
        body.to_string()
    };
    let tooltip = format!("battery {cap}% — {status}");
    format!(r#"{{"text":"{text}","class":"{class}","tooltip":"{tooltip}","alt":"{cap}"}}"#)
}

fn bucket_glyph(cap: u8) -> &'static str {
    match cap {
        0..=20 => BAT_EMPTY,
        21..=40 => BAT_QUARTER,
        41..=60 => BAT_HALF,
        61..=80 => BAT_THREE_Q,
        _ => BAT_FULL,
    }
}

/// Read the first `BAT*` directory by calling
/// [`sy_core::sensors::bat::sample()`]. The pure formatter
/// [`waybar_out`] expects `(capacity_pct, status_token)` where the
/// status token is the human-readable sysfs string (the tile renders
/// it verbatim in the tooltip and matches `== "Charging"` for the
/// bolt-prepend branch).
fn read_first_battery() -> Option<(u8, String)> {
    let snap = bat::sample()?;
    let first = snap.batteries.into_iter().next()?;
    Some((first.capacity_pct, status_token(first.status)))
}

/// Map the typed sensor variant back to the sysfs token the existing
/// waybar tile renders verbatim. Keeps the JSON tooltip byte-identical
/// to the pre-refactor output for `configs/waybar/*` consumers.
fn status_token(status: BatteryStatus) -> String {
    match status {
        BatteryStatus::Charging => "Charging",
        BatteryStatus::Discharging => "Discharging",
        BatteryStatus::Full => "Full",
        BatteryStatus::NotCharging => "Not charging",
        BatteryStatus::Unknown => "Unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goldens that lock the tile's Font Awesome glyph picks + the
    /// charging-prepended bolt. A `configs/waybar/*` consumer parses
    /// the `class` and `alt` fields directly, so any drift breaks the
    /// existing CSS.
    const GOLDEN_BAT_CHARGING: &str = include_str!("../tests/snapshots/waybar/bat-charging.json");
    const GOLDEN_BAT_DISCHARGING: &str =
        include_str!("../tests/snapshots/waybar/bat-discharging.json");
    const GOLDEN_BAT_ABSENT: &str = include_str!("../tests/snapshots/waybar/bat-absent.json");

    #[test]
    fn waybar_output_matches_snapshot_charging() {
        let out = waybar_out(Some((73, "Charging".to_string())));
        assert_eq!(format!("{out}\n"), GOLDEN_BAT_CHARGING);
    }

    #[test]
    fn waybar_output_matches_snapshot_discharging() {
        let out = waybar_out(Some((42, "Discharging".to_string())));
        assert_eq!(format!("{out}\n"), GOLDEN_BAT_DISCHARGING);
    }

    #[test]
    fn waybar_absent_matches_snapshot() {
        let out = waybar_out(None);
        assert_eq!(format!("{out}\n"), GOLDEN_BAT_ABSENT);
    }
}
