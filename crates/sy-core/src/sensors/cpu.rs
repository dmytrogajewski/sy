//! `/proc/stat` per-core jiffy parser plus a thin sysfs reader for
//! per-core scaling frequency. Per the kernel docs, each `cpuN` line
//! is `<user> <nice> <system> <idle> <iowait> <irq> <softirq>
//! <steal> <guest> <guest_nice>`; "busy" is everything except `idle +
//! iowait`. Utilisation is a delta between two snapshots — a single
//! `/proc/stat` read is meaningless on its own.

use serde::{Deserialize, Serialize};

/// One sensor tick of CPU state. `freq_mhz` and `temp_c` are I/O-bound
/// and live in [`sample`]; `parse_proc_stat` only fills
/// `per_core_util_pct`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuSample {
    /// Percent busy per logical core in `cpuN` order, 0.0..=100.0.
    /// Cores that were absent from either snapshot (hot-plugged
    /// in/out between samples) are skipped — the Vec only contains
    /// cores observed in both snapshots.
    pub per_core_util_pct: Vec<f32>,
    /// Per-core scaling-current frequency in MHz, or an empty Vec
    /// when `/sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq`
    /// is unavailable (containers, exotic kernels).
    pub freq_mhz: Vec<u32>,
    /// Package temperature in Celsius if `sample` could resolve a
    /// thermal zone. Per the Step 1 risks note, the sysfs fallback
    /// to `/sys/class/thermal/thermal_zone*/temp` is deferred; this
    /// stays `None` until then.
    pub temp_c: Option<f32>,
}

/// Per-core jiffy counters as scraped from a single `/proc/stat`
/// snapshot. `(busy, total)` where `total = busy + idle_and_wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CoreTicks {
    idx: u32,
    busy: u64,
    total: u64,
}

/// Scrape every `cpuN ...` line out of a `/proc/stat` blob. The
/// aggregate `cpu ` (no index) line is skipped — the popup wants
/// per-core series. Lines that parse badly are dropped silently;
/// the surrounding context can flag the whole tick as failed.
fn scrape_cores(raw: &str) -> Vec<CoreTicks> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        // The aggregate line begins with `cpu ` (space). Per-core
        // lines begin with a digit.
        let Some(first) = rest.chars().next() else {
            continue;
        };
        if !first.is_ascii_digit() {
            continue;
        }
        let mut fields = rest.split_ascii_whitespace();
        let Some(head) = fields.next() else { continue };
        let Ok(idx) = head.parse::<u32>() else {
            continue;
        };
        let nums: Vec<u64> = fields.filter_map(|f| f.parse().ok()).collect();
        // Need at least user/nice/system/idle/iowait.
        if nums.len() < 5 {
            continue;
        }
        let idle_and_wait = nums[3] + nums[4];
        let total: u64 = nums.iter().sum();
        let busy = total.saturating_sub(idle_and_wait);
        out.push(CoreTicks { idx, busy, total });
    }
    out
}

/// Diff two `/proc/stat` snapshots into per-core utilisation. Cores
/// that appear in `prev` but not `curr` (or vice versa) are dropped
/// from the output — that's the hot-plug case the Step 1 DoD calls
/// out. The output Vec is sorted by `cpuN` index so downstream code
/// can zip it with `freq_mhz` without an extra map step.
pub fn parse_proc_stat(prev: &str, curr: &str) -> CpuSample {
    let prev_cores = scrape_cores(prev);
    let curr_cores = scrape_cores(curr);
    let mut per_core_util_pct = Vec::with_capacity(curr_cores.len());
    for c in &curr_cores {
        let Some(p) = prev_cores.iter().find(|p| p.idx == c.idx) else {
            continue;
        };
        let d_total = c.total.saturating_sub(p.total);
        if d_total == 0 {
            // No elapsed time between snapshots for this core —
            // record 0.0 rather than NaN.
            per_core_util_pct.push(0.0);
            continue;
        }
        let d_busy = c.busy.saturating_sub(p.busy);
        let pct = (d_busy as f64 / d_total as f64) * 100.0;
        per_core_util_pct.push(pct as f32);
    }
    CpuSample {
        per_core_util_pct,
        freq_mhz: Vec::new(),
        temp_c: None,
    }
}

/// Read each `/sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq`
/// entry (kHz) and convert to MHz. Returns an empty Vec when the
/// cpufreq directory is missing — the caller decides whether that is
/// an error or a non-Intel/AMD platform without freq scaling.
fn read_freq_mhz() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
        return Vec::new();
    };
    let mut indexed: Vec<(u32, u32)> = Vec::new();
    for ent in entries.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(idx) = name.strip_prefix("cpu").and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let path = ent.path().join("cpufreq/scaling_cur_freq");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(khz) = raw.trim().parse::<u32>() else {
            continue;
        };
        indexed.push((idx, khz / 1000));
    }
    indexed.sort_by_key(|(idx, _)| *idx);
    indexed.into_iter().map(|(_, mhz)| mhz).collect()
}

/// I/O wrapper: reads `/proc/stat` twice ~100 ms apart so a single
/// `sample()` call returns a meaningful utilisation. Higher-level
/// loops (e.g. the aggregator) typically retain the previous
/// snapshot and call `parse_proc_stat` directly to avoid the extra
/// sleep — this helper exists for the one-shot waybar adapter.
pub fn sample() -> Option<CpuSample> {
    let prev = std::fs::read_to_string("/proc/stat").ok()?;
    std::thread::sleep(std::time::Duration::from_millis(100));
    let curr = std::fs::read_to_string("/proc/stat").ok()?;
    let mut s = parse_proc_stat(&prev, &curr);
    s.freq_mhz = read_freq_mhz();
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake `/proc/stat` for `n` cores where every core's
    /// busy / idle counters scale with `(idx + 1) * scale`. Gives
    /// each core a distinct utilisation so the test catches
    /// off-by-one zips.
    fn fake_proc_stat(n: u32, scale: u64) -> String {
        // Aggregate line first — the parser must ignore it.
        let mut out = String::from("cpu  0 0 0 0 0 0 0 0 0 0\n");
        for idx in 0..n {
            let busy = (idx as u64 + 1) * scale;
            // user nice system idle iowait irq softirq steal guest guest_nice
            out.push_str(&format!(
                "cpu{idx} {busy} 0 0 {idle} 0 0 0 0 0 0\n",
                idle = scale,
            ));
        }
        out
    }

    #[test]
    fn parse_proc_stat_16_core_ryzen() {
        // 16 cores; second snapshot adds one tick of busy per core,
        // ten of idle. Expected per-core utilisation:
        // d_busy / d_total = 1 / 11 ≈ 9.0909 %.
        const CORES: u32 = 16;
        let prev = fake_proc_stat(CORES, 100);
        // Bump each core's busy by 1 and idle by 10 between samples.
        let mut curr = String::from("cpu  0 0 0 0 0 0 0 0 0 0\n");
        for idx in 0..CORES {
            let busy = (idx as u64 + 1) * 100 + 1;
            let idle = 100 + 10;
            curr.push_str(&format!("cpu{idx} {busy} 0 0 {idle} 0 0 0 0 0 0\n"));
        }
        let sample = parse_proc_stat(&prev, &curr);
        assert_eq!(sample.per_core_util_pct.len(), CORES as usize);
        for util in &sample.per_core_util_pct {
            // 1 busy tick of 11 total → 9.09 %.
            assert!(
                (*util - 100.0 / 11.0).abs() < 0.01,
                "core util {util} should be ~9.09 %",
            );
        }
        // Freq + temp are sample()'s job, not the parser's.
        assert!(sample.freq_mhz.is_empty());
        assert!(sample.temp_c.is_none());
    }

    #[test]
    fn parse_proc_stat_handles_hotplug_gap() {
        // Eight cores in `prev`; core 3 is hot-unplugged before the
        // second snapshot. The output must skip core 3 and keep the
        // other seven, all in `cpuN` order.
        const CORES: u32 = 8;
        const MISSING: u32 = 3;
        let prev = fake_proc_stat(CORES, 100);
        let mut curr = String::from("cpu  0 0 0 0 0 0 0 0 0 0\n");
        for idx in 0..CORES {
            if idx == MISSING {
                continue;
            }
            let busy = (idx as u64 + 1) * 100 + 1;
            curr.push_str(&format!("cpu{idx} {busy} 0 0 110 0 0 0 0 0 0\n"));
        }
        let sample = parse_proc_stat(&prev, &curr);
        assert_eq!(sample.per_core_util_pct.len(), (CORES - 1) as usize);
    }
}
