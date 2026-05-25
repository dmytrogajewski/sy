//! `/proc/meminfo` parser. Each line is `<Key>:<spaces><value> kB`;
//! we want the MiB-scaled `total`, `used` (= `MemTotal -
//! MemAvailable`, the same definition `free(1)` switched to in 2014),
//! and `SwapTotal - SwapFree`.

use serde::{Deserialize, Serialize};

/// MiB-scaled memory snapshot. f32 would be lossy past ~16 TiB; u64
/// is overkill for a 128 GiB box but keeps the type stable if mon
/// ever lands on a workstation with more RAM than that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemSample {
    pub total_mib: u64,
    pub used_mib: u64,
    pub swap_used_mib: u64,
}

/// Pull a single `Key:` line's kB integer out of `/proc/meminfo`.
/// Returns `None` when the key is absent so the caller can decide
/// whether to drop the sample or zero-fill (mon does the former).
fn read_kb(raw: &str, key: &str) -> Option<u64> {
    for line in raw.lines() {
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        return rest
            .trim()
            .split_ascii_whitespace()
            .next()
            .and_then(|n| n.parse().ok());
    }
    None
}

/// Parse `/proc/meminfo` into MiB-scaled totals. Returns `None` if any
/// required key is missing; partial samples are worse than no sample.
pub fn parse_meminfo(raw: &str) -> Option<MemSample> {
    let total = read_kb(raw, "MemTotal")?;
    let available = read_kb(raw, "MemAvailable")?;
    let swap_total = read_kb(raw, "SwapTotal")?;
    let swap_free = read_kb(raw, "SwapFree")?;
    let used = total.saturating_sub(available);
    let swap_used = swap_total.saturating_sub(swap_free);
    Some(MemSample {
        total_mib: total / 1024,
        used_mib: used / 1024,
        swap_used_mib: swap_used / 1024,
    })
}

/// I/O wrapper around [`parse_meminfo`].
pub fn sample() -> Option<MemSample> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meminfo_with_swap() {
        // Trimmed-down snapshot from a Ryzen 9 / 64 GiB / 16 GiB swap
        // host. Numbers chosen to make total/used/swap distinct after
        // the kB→MiB divide.
        let raw = "\
MemTotal:       65536000 kB
MemFree:        10240000 kB
MemAvailable:   32768000 kB
Buffers:           12345 kB
SwapTotal:      16777216 kB
SwapFree:        8388608 kB
";
        let s = parse_meminfo(raw).expect("well-formed meminfo");
        // 65536000 kB / 1024 = 64000 MiB.
        assert_eq!(s.total_mib, 64000);
        // (65536000 - 32768000) / 1024 = 32000 MiB used.
        assert_eq!(s.used_mib, 32000);
        // (16777216 - 8388608) / 1024 = 8192 MiB swap used.
        assert_eq!(s.swap_used_mib, 8192);
    }
}
