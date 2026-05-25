//! `/proc/loadavg` parser. The line shape is
//! `<1m> <5m> <15m> <running/total> <last_pid>\n`. We only consume the
//! three load averages; the runqueue + pid fields are intentionally
//! dropped — `mon` already surfaces them through other sensors.

use serde::{Deserialize, Serialize};

/// Three classic Linux load averages, sampled in floats so the popup
/// can render decimals without a second division.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoadSample {
    pub one: f32,
    pub five: f32,
    pub fifteen: f32,
}

/// Parse the contents of `/proc/loadavg`. Returns `None` if the first
/// three whitespace-separated tokens are missing or unparseable; we
/// would rather emit an `errors[]` entry one level up than synthesise
/// a misleading zero.
pub fn parse_loadavg(raw: &str) -> Option<LoadSample> {
    let mut it = raw.split_ascii_whitespace();
    let one = it.next()?.parse().ok()?;
    let five = it.next()?.parse().ok()?;
    let fifteen = it.next()?.parse().ok()?;
    Some(LoadSample { one, five, fifteen })
}

/// I/O wrapper around [`parse_loadavg`]. Lives at the bottom of the
/// file by convention so the testable pure parser stays at the top.
pub fn sample() -> Option<LoadSample> {
    let raw = std::fs::read_to_string("/proc/loadavg").ok()?;
    parse_loadavg(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_loadavg_three_floats() {
        // Happy path + trailing newline, exactly the shape the kernel
        // emits. Values chosen so each slot is distinct.
        let raw = "0.42 1.07 2.31 3/812 12345\n";
        let s = parse_loadavg(raw).expect("well-formed /proc/loadavg");
        assert_eq!(s.one, 0.42);
        assert_eq!(s.five, 1.07);
        assert_eq!(s.fifteen, 2.31);
    }
}
