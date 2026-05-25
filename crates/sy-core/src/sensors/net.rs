//! `/proc/net/dev` parser. Each post-header line is
//! `<iface>: <rx_bytes> <rx_packets> <rx_errs> <rx_drop> <rx_fifo>
//! <rx_frame> <rx_compressed> <rx_multicast> <tx_bytes> <tx_packets>
//! <tx_errs> <tx_drop> <tx_fifo> <tx_colls> <tx_carrier> <tx_compressed>`.
//! We surface `rx_bytes` / `tx_bytes` per interface; the rest stays
//! latent until a panel asks for it.
//!
//! Rates (bit/s) are a delta between two snapshots — a single
//! `/proc/net/dev` read is meaningless on its own, exactly as with
//! `/proc/stat`.

use serde::{Deserialize, Serialize};

/// Cumulative rx/tx byte counters for one interface, scraped at one
/// instant. The aggregator subtracts two adjacent samples to derive
/// throughput; the popup panels read the throughput, not the raw
/// counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetInterface {
    /// Interface name as the kernel prints it (`lo`, `wlan0`,
    /// `enp4s0`, `docker0`, …).
    pub name: String,
    /// Total bytes received since boot (counter, monotonic modulo
    /// overflow).
    pub rx_bytes: u64,
    /// Total bytes transmitted since boot (counter, monotonic modulo
    /// overflow).
    pub tx_bytes: u64,
}

/// One sensor tick of network state — a flat list of all interfaces
/// observed in the read. Order matches `/proc/net/dev` (which is
/// stable in practice but the parser does not sort).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetSample {
    pub interfaces: Vec<NetInterface>,
}

/// Parse a `/proc/net/dev` blob into a list of per-interface byte
/// counters. The two header lines are dropped; malformed rows are
/// silently skipped — the surrounding tick can flag a wholesale read
/// failure but a single bad row should not poison the whole sample.
pub fn parse_proc_net_dev(raw: &str) -> NetSample {
    let mut interfaces = Vec::new();
    for line in raw.lines().skip(2) {
        let Some((head, rest)) = line.split_once(':') else {
            continue;
        };
        let name = head.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let nums: Vec<u64> = rest
            .split_ascii_whitespace()
            .filter_map(|f| f.parse().ok())
            .collect();
        // rx_bytes is field 0, tx_bytes is field 8.
        if nums.len() < 9 {
            continue;
        }
        interfaces.push(NetInterface {
            name,
            rx_bytes: nums[0],
            tx_bytes: nums[8],
        });
    }
    NetSample { interfaces }
}

/// I/O wrapper: reads `/proc/net/dev` once and hands the bytes to the
/// pure parser. Returns `None` only on a procfs read failure — an
/// empty `interfaces` Vec is a valid sample (no NICs configured).
pub fn sample() -> Option<NetSample> {
    let raw = std::fs::read_to_string("/proc/net/dev").ok()?;
    Some(parse_proc_net_dev(&raw))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_proc_net_dev() {
        // Captured from a laptop with loopback, wifi, and a bridge.
        // The two header lines are exactly as the kernel emits them.
        let raw = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 12345 100 0 0 0 0 0 0 12345 100 0 0 0 0 0 0
 wlan0: 99887766 1234 0 0 0 0 0 0 5544332 567 0 0 0 0 0 0
docker0: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
";
        let s = super::parse_proc_net_dev(raw);
        assert_eq!(s.interfaces.len(), 3);
        assert_eq!(s.interfaces[0].name, "lo");
        assert_eq!(s.interfaces[0].rx_bytes, 12345);
        assert_eq!(s.interfaces[0].tx_bytes, 12345);
        assert_eq!(s.interfaces[1].name, "wlan0");
        assert_eq!(s.interfaces[1].rx_bytes, 99_887_766);
        assert_eq!(s.interfaces[1].tx_bytes, 5_544_332);
        assert_eq!(s.interfaces[2].name, "docker0");
        assert_eq!(s.interfaces[2].rx_bytes, 0);
        assert_eq!(s.interfaces[2].tx_bytes, 0);
    }
}
