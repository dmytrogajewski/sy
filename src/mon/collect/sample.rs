//! Host-sensor sampling — one call returns a `HostSample` with cpu /
//! mem / load. The 1 Hz tick projects this into the ring buffer. Pure
//! sample reads live in `sy_core::sensors::*`; this module bundles the
//! three reads into one `HostSample` and gives the tick scheduler one
//! closure to schedule under `spawn_blocking`.

use sy_core::sensors::{cpu, disk, gpu_amd, gpu_nvidia, load, mem, net, npu_xdna};

/// One host-side sensor read. Each field is `Option` because a sensor
/// can legitimately fail on a host that lacks the corresponding sysfs
/// or procfs path (containers, exotic kernels); the tick records the
/// failure in `errors[]` and writes zero into the ring column.
#[derive(Debug, Default, Clone)]
pub struct HostSample {
    pub cpu: Option<cpu::CpuSample>,
    pub mem: Option<mem::MemSample>,
    pub load: Option<load::LoadSample>,
    pub net: Option<net::NetSample>,
    pub disk: Option<disk::DiskSample>,
    pub gpu_amd: gpu_amd::GpuAmdSnapshot,
    pub gpu_nvidia: gpu_nvidia::GpuNvidiaSnapshot,
    pub npu: Option<npu_xdna::NpuXdnaSample>,
}

/// Production sampler — reads cpu / mem / load / net / disk / gpu /
/// npu via the shared sensors crate. Blocking by design; callers wrap
/// it in `tokio::task::spawn_blocking` so the runtime stays responsive
/// while the ~100 ms cpu probe sleeps.
pub fn sample_host() -> HostSample {
    let npu = npu_xdna::sample();
    let npu = if npu.present { Some(npu) } else { None };
    HostSample {
        cpu: cpu::sample(),
        mem: mem::sample(),
        load: load::sample(),
        net: net::sample(),
        disk: disk::sample(),
        gpu_amd: gpu_amd::sample(),
        gpu_nvidia: gpu_nvidia::sample(),
        npu,
    }
}

/// Project a host sample into the ring buffer's f32 row. The column
/// inventory is intentionally minimal at Step 11 — Step 12's
/// snapshot-projection lands the full M-column shape. Reserved slots
/// past column 3 stay `0.0` so the ring file's on-disk shape (16
/// columns) is forward-compatible with the broader projection.
///
/// Layout (column index → metric):
///
/// | idx | metric                              |
/// |----:|-------------------------------------|
/// | 0   | cpu mean utilisation (percent)      |
/// | 1   | memory used (MiB)                   |
/// | 2   | swap used (MiB)                     |
/// | 3   | load average 1 m                    |
/// | 4-15| reserved (zero) for Step 12+        |
pub fn project_row(sample: &HostSample, n_metrics: usize) -> Vec<f32> {
    let mut row = vec![0.0_f32; n_metrics];
    if let Some(c) = &sample.cpu {
        if !c.per_core_util_pct.is_empty() {
            let mean = c.per_core_util_pct.iter().copied().sum::<f32>()
                / (c.per_core_util_pct.len() as f32);
            if n_metrics > 0 {
                row[0] = mean;
            }
        }
    }
    if let Some(m) = &sample.mem {
        if n_metrics > 1 {
            row[1] = m.used_mib as f32;
        }
        if n_metrics > 2 {
            row[2] = m.swap_used_mib as f32;
        }
    }
    if let Some(l) = &sample.load {
        if n_metrics > 3 {
            row[3] = l.one;
        }
    }
    row
}
