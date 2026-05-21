//! AMD XDNA (Ryzen AI NPU) reader.
//!
//! Step 3 ships the deterministic-zero stub. The real signal lives in
//! two places:
//!
//! 1. **Workload count** — comes from `aiplane::registry`. The full
//!    in-process tap (`intent::aiplane`) is wired in Step 6 and the
//!    snapshot assembler consumes it in Step 8. Until then, the
//!    sensor returns `workload_count: 0` so the daemon does not
//!    short-circuit on a missing channel.
//! 2. **Power telemetry (mW)** — exposed by the kernel as a DRM
//!    accelerator counter starting at kernel ≥ 7.1 (SPEC §4 + §6
//!    "kernel ≥ 7.1 unlocks NPU mW telemetry"). On older kernels we
//!    degrade to "0 mW" per SPEC.
//!
//! The sensor stays stateless and infallible — the daemon must not
//! lose a tick because the NPU happens to be idle. A future Step (8)
//! will inject the registry handle through this sensor's constructor.

use std::path::Path;

use anyhow::Result;

use super::{Sensor, SensorReading};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NpuReading {
    /// Number of in-flight workloads on the aiplane queue. Always 0
    /// until Step 6 wires the registry tap; Step 8 then routes the
    /// live depth into the snapshot.
    pub workload_count: u32,
    /// XDNA package power in milliwatts. 0 on kernel < 7.1 (no
    /// DRM-side counter); the real read lands when the user upgrades.
    pub mw: u32,
}

#[derive(Debug, Default)]
pub struct NpuSensor;

impl NpuSensor {
    pub fn new() -> Self {
        Self
    }
}

impl Sensor for NpuSensor {
    fn read(&self, _sysfs_root: &Path) -> Result<SensorReading> {
        // Deterministic zero until Step 6 (registry tap) + Step 8
        // (kernel ≥ 7.1 mW counter via amdxdna DRM). Returning Ok here
        // — not an error — is load-bearing: the snapshot must always
        // populate every channel so the bandit's feature vec stays
        // dimensionally stable across kernels.
        Ok(SensorReading::Npu(NpuReading {
            workload_count: 0,
            mw: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Roadmap §3 test: the aiplane registry tap is deferred to
    /// Step 8. Until then the sensor must return a zero workload
    /// count, *not* an error — the daemon would otherwise drop the
    /// tick.
    #[test]
    fn workload_count_zero_without_registry() {
        let r = NpuSensor::new()
            .read(&PathBuf::from("/nonexistent"))
            .expect("npu read must never fail in the stub path");
        match r {
            SensorReading::Npu(n) => {
                assert_eq!(n.workload_count, 0);
                assert_eq!(n.mw, 0);
            }
            other => panic!("expected Npu reading, got {other:?}"),
        }
    }
}
