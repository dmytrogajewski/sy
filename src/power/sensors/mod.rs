//! Hardware sensors for `sy power` — Step 2 lands the first three
//! (`pstate`, `platform`, `hwmon`); Step 3 adds RAPL / iGPU / NPU /
//! battery; Step 8 wires the full set into the snapshot assembler.
//!
//! Every sensor takes a `sysfs_root: &Path` so tests can point at
//! `src/power/fixtures/sys/<board>/` and so the daemon can be tested
//! against a tmpdir snapshot. Production callers pass
//! `Path::new("/sys")`.

use std::path::Path;

use anyhow::Result;

pub mod battery;
pub mod hwmon;
pub mod igpu;
pub mod npu;
pub mod platform;
pub mod pstate;
pub mod rapl;

pub use battery::{BatteryReading, BatterySensor};
pub use hwmon::{HwmonReading, HwmonSensor};
pub use igpu::{IgpuReading, IgpuSensor};
pub use npu::{NpuReading, NpuSensor};
pub use platform::{PlatformReading, PlatformSensor};
pub use pstate::{PstateReading, PstateSensor};
pub use rapl::{RaplReading, RaplSensor};

/// One reading per sensor kind. `Blocked` is reserved for levers the
/// kernel silently ignores (e.g. EPP writes while `amd_dynamic_epp=enable`)
/// so the daemon can surface the no-op explicitly rather than report a
/// stale value as if it had been honoured.
#[derive(Debug, Clone, PartialEq)]
pub enum SensorReading {
    Pstate(PstateReading),
    Platform(PlatformReading),
    Hwmon(HwmonReading),
    Rapl(RaplReading),
    Igpu(IgpuReading),
    Npu(NpuReading),
    Battery(BatteryReading),
    Blocked,
}

/// Read a hardware fact from a sysfs root. Implementors are
/// stateless; the `sysfs_root` is injected per-call so the same impl
/// runs against `/sys` in prod and `src/power/fixtures/sys/hx370/` in
/// tests.
pub trait Sensor {
    fn read(&self, sysfs_root: &Path) -> Result<SensorReading>;
}
