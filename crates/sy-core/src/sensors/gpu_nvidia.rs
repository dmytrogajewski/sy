//! `nvidia-smi --query-gpu=… --format=csv,noheader,nounits` parser
//! for NVIDIA dGPUs. The CLI invocation lives in [`sample()`] with a
//! **fixed argv vector** (no user-supplied fragments) per SPEC §4
//! non-functional security; the pure parser
//! [`parse_smi_csv`] takes stdout as `&str` so tests don't need the
//! binary on the path.
//!
//! The query columns are `index, name, utilization.gpu, memory.used,
//! memory.total, temperature.gpu, power.draw`. `--format=csv,
//! noheader,nounits` strips the header row and unit suffixes so every
//! cell is a bare number (or a free-form name string for column 1).
//! `memory.used` and `memory.total` are reported in MiB.

use std::process::Command;

use serde::{Deserialize, Serialize};

/// One NVIDIA GPU's worth of state at one instant. Mirrors the seven
/// `--query-gpu` columns; every numeric field is `Option` so a
/// driver that refuses to report (`[Not Supported]`, `N/A`) lands as
/// `None` without dropping the row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuNvidiaSample {
    /// GPU index as nvidia-smi numbers them (0-based).
    pub index: u32,
    /// Product name string (e.g. `NVIDIA GeForce RTX 4090`).
    pub name: String,
    /// Compute utilisation 0..=100 %.
    pub util_pct: Option<u8>,
    /// VRAM in use, in MiB.
    pub vram_used_mib: Option<u64>,
    /// VRAM total, in MiB.
    pub vram_total_mib: Option<u64>,
    /// Edge temperature in Celsius.
    pub temp_c: Option<f32>,
    /// Average board power draw in watts.
    pub power_w: Option<f32>,
}

/// One sensor tick of NVIDIA GPU state — one row per GPU. Empty
/// `gpus` Vec means `nvidia-smi` was missing or returned no rows
/// (the typical AMD-only / no-NVIDIA case).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuNvidiaSnapshot {
    pub gpus: Vec<GpuNvidiaSample>,
}

/// Pure parser: take the `nvidia-smi --format=csv,noheader,nounits`
/// stdout blob and produce one [`GpuNvidiaSample`] per row. Rows that
/// fail to produce an `index` or `name` are dropped; rows that
/// produce those two but fail to parse a numeric column emit `None`
/// for that column.
pub fn parse_smi_csv(raw: &str) -> GpuNvidiaSnapshot {
    let mut gpus = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        // Seven columns expected; reject rows that don't match shape.
        const EXPECTED_COLS: usize = 7;
        if cols.len() != EXPECTED_COLS {
            continue;
        }
        let Ok(index) = cols[0].parse::<u32>() else {
            continue;
        };
        let name = cols[1].to_string();
        if name.is_empty() {
            continue;
        }
        gpus.push(GpuNvidiaSample {
            index,
            name,
            util_pct: cols[2].parse::<u8>().ok(),
            vram_used_mib: cols[3].parse::<u64>().ok(),
            vram_total_mib: cols[4].parse::<u64>().ok(),
            temp_c: cols[5].parse::<f32>().ok(),
            power_w: cols[6].parse::<f32>().ok(),
        });
    }
    GpuNvidiaSnapshot { gpus }
}

/// Fixed argv for the `nvidia-smi` query — per SPEC §4 security
/// requirement no user input flows into the command line. The columns
/// are documented at [NVIDIA-SMI Properties].
///
/// [NVIDIA-SMI Properties]: https://developer.nvidia.com/nvidia-system-management-interface
const NVIDIA_SMI_ARGS: [&str; 2] = [
    "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
    "--format=csv,noheader,nounits",
];

/// I/O wrapper: spawn `nvidia-smi` with `NVIDIA_SMI_ARGS` and feed
/// its stdout into [`parse_smi_csv`]. Returns an empty snapshot if
/// the binary is missing or exits non-zero — both indicate "no
/// NVIDIA GPU here", not a sensor failure.
pub fn sample() -> GpuNvidiaSnapshot {
    let Ok(out) = Command::new("nvidia-smi").args(NVIDIA_SMI_ARGS).output() else {
        return GpuNvidiaSnapshot { gpus: Vec::new() };
    };
    if !out.status.success() {
        return GpuNvidiaSnapshot { gpus: Vec::new() };
    }
    let body = String::from_utf8_lossy(&out.stdout);
    parse_smi_csv(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_smi_csv_single_gpu() {
        // One row, all values present. Whitespace after commas is
        // nvidia-smi's default and the parser must tolerate it.
        let raw = "0, NVIDIA GeForce RTX 4090, 42, 8192, 24576, 61, 235.5\n";
        let s = parse_smi_csv(raw);
        assert_eq!(s.gpus.len(), 1);
        let g = &s.gpus[0];
        assert_eq!(g.index, 0);
        assert_eq!(g.name, "NVIDIA GeForce RTX 4090");
        assert_eq!(g.util_pct, Some(42));
        assert_eq!(g.vram_used_mib, Some(8192));
        assert_eq!(g.vram_total_mib, Some(24576));
        assert_eq!(g.temp_c, Some(61.0));
        assert_eq!(g.power_w, Some(235.5));
    }

    #[test]
    fn parse_smi_csv_dual_gpu() {
        // Two rows; second card's util column is the nvidia-smi
        // sentinel `[Not Supported]` — must land as `None` without
        // dropping the row.
        let raw = "0, NVIDIA RTX A6000, 17, 4096, 49140, 55, 90.2\n\
                   1, NVIDIA RTX A6000, [Not Supported], 0, 49140, 50, 12.0\n";
        let s = parse_smi_csv(raw);
        assert_eq!(s.gpus.len(), 2);
        assert_eq!(s.gpus[0].index, 0);
        assert_eq!(s.gpus[0].util_pct, Some(17));
        assert_eq!(s.gpus[1].index, 1);
        assert!(s.gpus[1].util_pct.is_none());
        assert_eq!(s.gpus[1].vram_used_mib, Some(0));
        assert_eq!(s.gpus[1].power_w, Some(12.0));
    }
}
