//! AMD Ryzen AI / XDNA NPU sensor. Versioned per SPEC §6 risk
//! "AMD XDNA sysfs path renames in Linux 7.x":
//!
//! - `NpuSource::V1` — current `/sys/class/accel/accel0/device/...`
//!   layout. Util % is a delta of the kernel's pm_runtime counters
//!   (`runtime_active_time`, `runtime_suspended_time`) between two
//!   samples; the first tick after boot has no prior pair and falls
//!   back to a binary read of `power_state` (`D0` → 100, else 0).
//! - `NpuSource::AmdgpuTopFallback` — `amdgpu_top --xdna --json` is
//!   the documented escape hatch when the sysfs schema renames out
//!   from under us. The parser only looks for the keys it needs
//!   (`util_pct`, optional `power_w`, optional `fw_version`) so a
//!   future amdgpu_top JSON shape change won't take down v1.
//!
//! Future `NpuSource::V2` (a second sysfs schema) slots in here when
//! a kernel rename actually forces it (SPEC ROADMAP §22 anti-goal).
//!
//! The two pure parsers ([`parse_pm_runtime`], [`parse_amdgpu_top_xdna`])
//! do no I/O; [`sample()`] owns the procfs / sysfs reads, the
//! pm_runtime cache file, and the `/proc/*/fd/*` holders walk.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Which data path produced this sample. Lets doctor / panel UI
/// surface "we're on the fallback because sysfs changed" without an
/// out-of-band channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NpuSource {
    /// Linux 7.0/7.1-era `/sys/class/accel/accel0/...` layout.
    V1,
    /// `amdgpu_top --xdna --json` fallback when V1 sysfs paths fail.
    AmdgpuTopFallback,
}

/// One sensor tick of NPU state. `holders` and `bdf` are I/O-bound
/// (procfs walk + sysfs symlink) and live in [`sample()`]; the pure
/// parsers leave them empty / `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NpuXdnaSample {
    /// `true` when a usable data source produced this sample (V1 or
    /// the fallback). `false` means no NPU detected.
    pub present: bool,
    /// Active = D0 power state, the kernel's "doing work" flag.
    pub active: bool,
    /// Utilisation percent over the tick interval, 0..=100.
    pub util_pct: u32,
    /// XDNA firmware version string when readable.
    pub fw_version: Option<String>,
    /// `comm` of each process holding `/dev/accel/accel0`. Lifted
    /// from `src/npu.rs::find_holders` in [`sample()`].
    pub holders: Vec<String>,
    /// PCI BDF (`c5:00.1`) of the NPU function, or `None` when the
    /// symlink couldn't be resolved.
    pub bdf: Option<String>,
    /// Power draw in watts; the V1 sysfs path doesn't expose this so
    /// it stays `None` until the fallback or a future V2 fills it.
    pub power_w: Option<f32>,
    /// Which data source produced this sample.
    pub source: NpuSource,
}

/// Path to the accel char device. Used both as the presence check
/// and as the symlink target the holders walk matches against.
pub const ACCEL_DEV: &str = "/dev/accel/accel0";
const POWER_STATE_PATH: &str = "/sys/class/accel/accel0/device/power_state";
const FW_VERSION_PATH: &str = "/sys/class/accel/accel0/device/fw_version";
const RUNTIME_ACTIVE: &str = "/sys/class/accel/accel0/device/power/runtime_active_time";
const RUNTIME_SUSPENDED: &str = "/sys/class/accel/accel0/device/power/runtime_suspended_time";

/// Pure: compute util % from a pair of `(active, suspended)` counter
/// readings. `prev` is `None` on the first tick after boot / cache
/// loss, in which case we fall back to the binary `active_now`
/// reading (100 if D0, else 0). Counter wraparound or reset is
/// handled by saturating subtraction — a reset looks like "no
/// elapsed time" and we again fall back to `active_now`.
pub fn parse_pm_runtime(prev: Option<(u64, u64)>, curr: (u64, u64), active_now: bool) -> u32 {
    let Some((p_active, p_suspended)) = prev else {
        return if active_now { 100 } else { 0 };
    };
    let (c_active, c_suspended) = curr;
    // Counters are u64 monotonic per kernel docs but a runtime PM
    // reset (suspend cycle, driver reload) zeros them. Saturating
    // sub treats that as "zero delta" and we fall back to
    // active_now.
    let d_active = c_active.saturating_sub(p_active);
    let d_suspended = c_suspended.saturating_sub(p_suspended);
    let total = d_active + d_suspended;
    if total == 0 {
        return if active_now { 100 } else { 0 };
    }
    ((d_active * 100) / total).min(100) as u32
}

/// Pure: parse an `amdgpu_top --xdna --json` blob into a sample. The
/// parser only requires `util_pct` — every other field is best-effort
/// so a future amdgpu_top JSON shape change can't take down the
/// whole sensor. Returns `None` when the JSON doesn't carry a
/// recognisable util reading.
pub fn parse_amdgpu_top_xdna(raw: &str) -> Option<NpuXdnaSample> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    // amdgpu_top emits either an object with an `xdna` key or a
    // top-level object describing the NPU directly. Probe both.
    let xdna = v.get("xdna").unwrap_or(&v);
    let util_pct = xdna
        .get("util_pct")
        .and_then(|n| n.as_u64())
        .or_else(|| xdna.get("utilization").and_then(|n| n.as_u64()))?
        .min(100) as u32;
    let power_w = xdna
        .get("power_w")
        .and_then(|n| n.as_f64())
        .map(|w| w as f32);
    let fw_version = xdna
        .get("fw_version")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string());
    let active = xdna
        .get("active")
        .and_then(|n| n.as_bool())
        .unwrap_or(util_pct > 0);
    Some(NpuXdnaSample {
        present: true,
        active,
        util_pct,
        fw_version,
        holders: Vec::new(),
        bdf: None,
        power_w,
        source: NpuSource::AmdgpuTopFallback,
    })
}

/// V1 sysfs read: `(runtime_active_time, runtime_suspended_time)`
/// counters as `(u64, u64)`. Lives in the I/O section so callers can
/// stub the pair when testing [`parse_pm_runtime`].
fn read_pm_counters() -> Option<(u64, u64)> {
    let a = std::fs::read_to_string(RUNTIME_ACTIVE).ok()?;
    let s = std::fs::read_to_string(RUNTIME_SUSPENDED).ok()?;
    let av: u64 = a.trim().parse().ok()?;
    let sv: u64 = s.trim().parse().ok()?;
    Some((av, sv))
}

fn parse_cache(raw: &str) -> Option<(u64, u64)> {
    let mut it = raw.split_whitespace();
    let a: u64 = it.next()?.parse().ok()?;
    let s: u64 = it.next()?.parse().ok()?;
    Some((a, s))
}

fn cache_path() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("sy-npu.last");
        }
    }
    // Fall back to /tmp when XDG_RUNTIME_DIR is unset. Avoids the
    // libc::getuid() unsafe call that the legacy waybar module
    // carried — sy-core stays `unsafe`-free.
    PathBuf::from("/tmp/sy-npu.last")
}

fn read_bdf() -> Option<String> {
    let link = std::fs::read_link("/sys/class/accel/accel0/device").ok()?;
    Some(
        link.file_name()?
            .to_string_lossy()
            .trim_start_matches("0000:")
            .to_string(),
    )
}

/// `/proc/*/fd/*` walk: every process that has `/dev/accel/accel0`
/// open by `comm`. Sorted + deduped so the popup gets a stable list.
fn find_holders() -> Vec<String> {
    let mut holders = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return holders;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if !n.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid = n.into_owned();
        let fd_dir: PathBuf = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        let mut hit = false;
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                if target.to_string_lossy() == ACCEL_DEV {
                    hit = true;
                    break;
                }
            }
        }
        if hit {
            let comm = std::fs::read_to_string(entry.path().join("comm"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| format!("pid {pid}"));
            holders.push(comm);
        }
    }
    holders.sort();
    holders.dedup();
    holders
}

/// Try the V1 sysfs path. Returns `None` when any V1 read fails — the
/// caller then dispatches the [`NpuSource::AmdgpuTopFallback`] path.
fn try_v1() -> Option<NpuXdnaSample> {
    if !Path::new(ACCEL_DEV).exists() {
        return None;
    }
    let curr = read_pm_counters()?;
    let active = std::fs::read_to_string(POWER_STATE_PATH)
        .map(|v| v.trim() == "D0")
        .ok()
        .unwrap_or(false);
    let cache = cache_path();
    let prev = std::fs::read_to_string(&cache)
        .ok()
        .as_deref()
        .and_then(parse_cache);
    // Persist the new pair; cache I/O is non-fatal.
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&cache, format!("{} {}\n", curr.0, curr.1));
    let util_pct = parse_pm_runtime(prev, curr, active);
    let fw_version = std::fs::read_to_string(FW_VERSION_PATH)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(NpuXdnaSample {
        present: true,
        active,
        util_pct,
        fw_version,
        holders: Vec::new(),
        bdf: read_bdf(),
        power_w: None,
        source: NpuSource::V1,
    })
}

/// Run `amdgpu_top --xdna --json` and feed stdout into
/// [`parse_amdgpu_top_xdna`]. Returns `None` if the binary is
/// missing or the JSON parses without a recognisable util reading.
fn try_amdgpu_top() -> Option<NpuXdnaSample> {
    let out = Command::new("amdgpu_top")
        .args(["--xdna", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout);
    parse_amdgpu_top_xdna(&body)
}

/// I/O wrapper. Dispatches V1 → fallback per the versioning policy
/// above. The holders walk only fires once a source has succeeded —
/// no point listing `/dev/accel/accel0` holders on a box without an
/// NPU. Returns an absent (`present=false`) sample when both V1 and
/// the fallback fail; that's the "no NPU detected" case, indistinguish-
/// able from "fallback also broken" so doctor needs a separate probe.
pub fn sample() -> NpuXdnaSample {
    let mut s = try_v1().or_else(try_amdgpu_top).unwrap_or(NpuXdnaSample {
        present: false,
        active: false,
        util_pct: 0,
        fw_version: None,
        holders: Vec::new(),
        bdf: None,
        power_w: None,
        source: NpuSource::V1,
    });
    if s.present {
        s.holders = find_holders();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pm_runtime_first_tick() {
        // No prior cache (boot, cache cleared) → fall back to the
        // binary active_now reading. D0 → 100, D3 → 0.
        let curr = (1_000, 5_000);
        assert_eq!(parse_pm_runtime(None, curr, true), 100);
        assert_eq!(parse_pm_runtime(None, curr, false), 0);
    }

    #[test]
    fn parse_pm_runtime_wraparound() {
        // Driver reload between samples zeros the counters; the
        // second sample is "smaller" than the first. Saturating sub
        // makes the delta zero, so we fall back to active_now.
        let prev = Some((10_000, 50_000));
        let curr = (100, 200); // counter reset
        assert_eq!(parse_pm_runtime(prev, curr, false), 0);
        assert_eq!(parse_pm_runtime(prev, curr, true), 100);
    }

    #[test]
    fn parse_pm_runtime_normal_window() {
        // Real tick: 250 ms active, 750 ms suspended → 25 % util.
        let prev = Some((1_000, 5_000));
        let curr = (1_250, 5_750);
        assert_eq!(parse_pm_runtime(prev, curr, true), 25);
    }

    #[test]
    fn amdgpu_top_fallback_when_v1_fails() {
        // Canned JSON shaped like `amdgpu_top --xdna --json` output:
        // an `xdna` object with util / power / firmware fields. The
        // pure parser is the fallback surface — `sample()` would
        // call this after a V1 sysfs error.
        let raw = r#"{
            "xdna": {
                "util_pct": 47,
                "power_w": 2.3,
                "fw_version": "1.5.9",
                "active": true
            }
        }"#;
        let s = parse_amdgpu_top_xdna(raw).expect("fallback must parse");
        assert!(s.present);
        assert_eq!(s.util_pct, 47);
        assert_eq!(s.power_w, Some(2.3));
        assert_eq!(s.fw_version.as_deref(), Some("1.5.9"));
        assert!(s.active);
        assert_eq!(s.source, NpuSource::AmdgpuTopFallback);
    }

    #[test]
    fn amdgpu_top_fallback_tolerates_missing_optional_fields() {
        // The util is the only must-have; the fallback's whole point
        // is to keep working when amdgpu_top's JSON shape shifts.
        let raw = r#"{"xdna": {"util_pct": 12}}"#;
        let s = parse_amdgpu_top_xdna(raw).expect("util-only must parse");
        assert_eq!(s.util_pct, 12);
        assert!(s.power_w.is_none());
        assert!(s.fw_version.is_none());
        // Active falls back to "util > 0" when the JSON doesn't say.
        assert!(s.active);
    }

    #[test]
    fn amdgpu_top_fallback_rejects_unrecognisable_json() {
        assert!(parse_amdgpu_top_xdna(r#"{"unrelated": 1}"#).is_none());
        assert!(parse_amdgpu_top_xdna("not json at all").is_none());
    }
}
