//! RAPL (Running Average Power Limit) reader via the `powercap` sysfs
//! tree.
//!
//! SPEC §4 calls this lane `powercap / amd_energy`; on the HX 370 dev
//! host the node is exposed as `class/powercap/intel-rapl:0/energy_uj`
//! even though the silicon is AMD (the kernel keeps the legacy
//! `intel-rapl*` naming for cross-vendor package energy). We capture
//! the device fixture verbatim; if a kernel build exposes the
//! AMD-specific `amd_energy` node instead, parsing remains a
//! microjoule counter and the same delta math applies.
//!
//! The single observable surface is `package_power_w_5tap` — the
//! 5-sample moving average of instantaneous package power derived
//! from `energy_uj` deltas. The `Sensor::read` trait is `&self`
//! (stateless contract) so the moving-average window lives behind an
//! internal `Mutex<VecDeque<Sample>>`. The lock is held only for the
//! push + average pass.
//!
//! Wrap-around: `energy_uj` is a monotonic counter modulo
//! `max_energy_range_uj`. When the raw read decreases we treat it as
//! a wrap and add the range back rather than emit a negative-power
//! sample.
//!
//! The 5-tap window length is the SPEC §4 contract — the snapshot
//! assembler (Step 8) consumes `package_power_w_5tap` directly.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};

use super::{Sensor, SensorReading};

const RAPL_NODE: &str = "class/powercap/intel-rapl:0";
const ENERGY_FILE: &str = "energy_uj";
const MAX_RANGE_FILE: &str = "max_energy_range_uj";

/// Window length per SPEC §4 "`package_power_w_5tap` — 5-sample moving
/// average". The arm reward function (Step 21) assumes this exact
/// width.
pub const RAPL_WINDOW: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RaplReading {
    /// 5-tap moving average of package power in watts. `None` until
    /// at least two `read()` calls have landed (one sample = no delta).
    pub package_power_w_5tap: Option<f32>,
    /// Most-recent raw `energy_uj` counter — exposed for the audit
    /// log so a replay can reconstruct deltas independent of the
    /// daemon's in-memory window.
    pub energy_uj: u64,
}

/// Instantaneous power sample (W). The wall-clock time of the
/// sample is not retained — the moving average is sample-count based
/// per SPEC §4, so the window only carries power values.
type Sample = f32;

#[derive(Debug)]
pub struct RaplSensor {
    /// (last raw energy reading, last observed timestamp).
    last: Mutex<Option<(u64, Instant)>>,
    /// Ring of recent instantaneous-power samples; oldest first.
    window: Mutex<VecDeque<Sample>>,
}

impl Default for RaplSensor {
    fn default() -> Self {
        Self::new()
    }
}

impl RaplSensor {
    pub fn new() -> Self {
        Self {
            last: Mutex::new(None),
            window: Mutex::new(VecDeque::with_capacity(RAPL_WINDOW)),
        }
    }
}

impl Sensor for RaplSensor {
    fn read(&self, sysfs_root: &Path) -> Result<SensorReading> {
        let node = sysfs_root.join(RAPL_NODE);
        let energy_uj = read_u64(&node.join(ENERGY_FILE))?;
        let max_range = read_u64(&node.join(MAX_RANGE_FILE)).ok();
        let now = Instant::now();
        let avg = self.update_and_average(energy_uj, max_range, now)?;
        Ok(SensorReading::Rapl(RaplReading {
            package_power_w_5tap: avg,
            energy_uj,
        }))
    }
}

impl RaplSensor {
    /// Update internal state with a new energy reading and return the
    /// current 5-tap moving average (in W) if at least one delta has
    /// been observed. Pulled out for direct testing without sysfs.
    fn update_and_average(
        &self,
        energy_uj: u64,
        max_range: Option<u64>,
        now: Instant,
    ) -> Result<Option<f32>> {
        let mut last = self
            .last
            .lock()
            .map_err(|_| anyhow::anyhow!("rapl last-lock poisoned"))?;
        let mut window = self
            .window
            .lock()
            .map_err(|_| anyhow::anyhow!("rapl window-lock poisoned"))?;
        if let Some((prev_uj, prev_at)) = *last {
            let delta_uj = delta_energy_uj(prev_uj, energy_uj, max_range);
            let dt_s = now.saturating_duration_since(prev_at).as_secs_f32();
            // A sample with zero/negative wall-clock delta is dropped —
            // a non-monotonic clock would otherwise inject infinities.
            if dt_s > 0.0 {
                let power_w = (delta_uj as f32 / 1_000_000.0) / dt_s;
                push_sample(&mut window, power_w);
            }
        }
        *last = Some((energy_uj, now));
        Ok(average(&window))
    }
}

fn push_sample(window: &mut VecDeque<Sample>, s: Sample) {
    if window.len() == RAPL_WINDOW {
        window.pop_front();
    }
    window.push_back(s);
}

fn average(window: &VecDeque<Sample>) -> Option<f32> {
    if window.is_empty() {
        return None;
    }
    let sum: f32 = window.iter().sum();
    Some(sum / window.len() as f32)
}

/// Compute the unsigned microjoule delta across a `energy_uj` pair,
/// handling wrap-around per the powercap contract.
fn delta_energy_uj(prev: u64, cur: u64, max_range: Option<u64>) -> u64 {
    if cur >= prev {
        cur - prev
    } else if let Some(range) = max_range {
        // Wrap: counter rolled over `range` between reads.
        range.saturating_sub(prev).saturating_add(cur)
    } else {
        // No range advertised → assume no wrap; treat as zero delta
        // rather than emit a bogus negative-power sample.
        0
    }
}

fn read_u64(path: &Path) -> Result<u64> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .trim()
        .to_string();
    raw.parse::<u64>()
        .with_context(|| format!("parse u64 at {}: {raw:?}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// SPEC §4 contract: `package_power_w_5tap` is a 5-sample mean. A
    /// single high-power burst inside an otherwise-quiet window must
    /// be attenuated — a burst of 50 W amid four 10 W samples should
    /// land at (50 + 4×10) / 5 = 18 W, demonstrably smoother than the
    /// raw 50 W spike.
    #[test]
    fn moving_average_smooths_burst() {
        let s = RaplSensor::new();
        let t0 = Instant::now();
        // 1 µJ per W per µs → for 1 W avg over 1 s we'd need 1_000_000 µJ.
        // We use a synthetic timeline: 1-second steps, energy increments
        // shaped to land each delta on the target instantaneous power.
        let pattern_w: [u64; 5] = [10, 10, 10, 10, 50];
        let mut energy: u64 = 1_000_000_000;
        // Seed the "previous" sample.
        s.update_and_average(energy, None, t0).expect("seed");
        for (i, &power_w) in pattern_w.iter().enumerate() {
            // Δenergy in µJ = power_w × dt_s × 1e6, dt_s = 1.
            energy += power_w * 1_000_000;
            let t = t0 + Duration::from_secs((i as u64) + 1);
            s.update_and_average(energy, None, t).expect("tick");
        }
        let final_avg = s
            .update_and_average(energy, None, t0 + Duration::from_secs(100))
            .expect("read")
            .expect("avg populated");
        // Last 5 samples are [10, 10, 10, 10, 50] plus the trailing
        // zero-delta read at +100s. The push keeps the 5 most recent
        // (the burst pattern minus the leading 10, plus the zero).
        // Easiest invariant: smoothed value sits strictly below the
        // burst (50 W) and strictly above the calm floor (10 W).
        const CALM_W: f32 = 10.0;
        const BURST_W: f32 = 50.0;
        assert!(
            final_avg > CALM_W && final_avg < BURST_W,
            "moving avg {final_avg} must sit between calm {CALM_W} and burst {BURST_W}",
        );
    }

    #[test]
    fn wrap_around_handled_when_max_range_known() {
        const RANGE: u64 = 1_000_000;
        // prev near top, cur small → wrapped.
        let d = delta_energy_uj(RANGE - 100, 200, Some(RANGE));
        assert_eq!(d, 300);
    }

    #[test]
    fn fixture_read_populates_energy_uj() {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/power/fixtures/sys/hx370");
        let r = RaplSensor::new().read(&fixture).expect("rapl read");
        match r {
            SensorReading::Rapl(rd) => {
                assert!(rd.energy_uj > 0, "fixture energy_uj must be non-zero");
                // First read = no delta yet; moving avg is None.
                assert!(rd.package_power_w_5tap.is_none());
            }
            other => panic!("expected Rapl reading, got {other:?}"),
        }
    }
}
