//! Canonical wire shape for a `sy mon` system snapshot.
//!
//! Per SPEC §4 "SystemSnapshot JSON schema" + D-SCHEMA, every panel of
//! the popup, every IPC frame on `system.mon.{snapshot,subscribe}`,
//! and every MCP `system.mon.snapshot` response carries a
//! [`SystemSnapshot`]. The schema is versioned: bumps require a
//! `CHANGELOG.md` entry and a deprecation notice (SPEC §4 "Migration
//! & compatibility").
//!
//! The aggregator that *populates* this struct lands in ROADMAP Step
//! 11+. This module is type-only on purpose — the wire shape and the
//! sensor read shape (`crates/sy-core/src/sensors/*Sample`) are
//! deliberately not coupled so the two can evolve independently.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

/// Schema version transported in [`SystemSnapshot::schema_version`].
/// Breaking field renames or removals bump this; additive fields do
/// not. Consumers MAY refuse to parse a snapshot with a higher
/// version than they know.
pub const SCHEMA_VERSION: u32 = 1;

/// One coherent slice of the machine's state at a single instant.
///
/// The field order is the wire order (serde serialises in declaration
/// order); deviating from SPEC §4 here would silently break agent
/// scripts that diff snapshot output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemSnapshot {
    /// See [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Capture instant in Unix milliseconds. The aggregator stamps
    /// this once per tick after every panel has been collected.
    pub captured_at_ms: u64,
    pub cpu: CpuPanel,
    pub mem: MemPanel,
    /// One entry per physical GPU. Empty on a host without any
    /// discoverable GPU (DRM card absent / `nvidia-smi` missing).
    pub gpu: Vec<GpuPanel>,
    pub npu: NpuPanel,
    /// One entry per network interface visible in `/proc/net/dev`.
    pub net: Vec<NetIfacePanel>,
    /// One entry per block device visible in `/proc/diskstats`.
    pub disk: Vec<DiskDevicePanel>,
    pub aiplane: AiplanePanel,
    pub knowledge: KnowledgePanel,
    pub agents: AgentsPanel,
    pub power: PowerPanel,
    pub supervisor: SupervisorPanel,
    /// Per-source errors observed by the aggregator during the tick
    /// (sensor read failure, plane socket missing, scrape timeout).
    /// Empty on a fully healthy tick.
    pub errors: Vec<MonError>,
}

impl Default for SystemSnapshot {
    /// Returns an empty snapshot stamped with the current
    /// [`SCHEMA_VERSION`]. All panels are at their type's zero shape
    /// so the aggregator can build a snapshot incrementally and emit
    /// what it has even if some sensors failed.
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            captured_at_ms: 0,
            cpu: CpuPanel::default(),
            mem: MemPanel::default(),
            gpu: Vec::new(),
            npu: NpuPanel::default(),
            net: Vec::new(),
            disk: Vec::new(),
            aiplane: AiplanePanel::default(),
            knowledge: KnowledgePanel::default(),
            agents: AgentsPanel::default(),
            power: PowerPanel::default(),
            supervisor: SupervisorPanel::default(),
            errors: Vec::new(),
        }
    }
}

/// Host CPU panel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuPanel {
    /// Percent busy per logical core in `cpuN` order, 0.0..=100.0.
    pub per_core_util_pct: Vec<f32>,
    /// Per-core scaling-current frequency in MHz, same order as
    /// `per_core_util_pct`.
    pub freq_mhz: Vec<u32>,
    /// Package temperature in Celsius. Zero when the host has no
    /// resolvable thermal zone; the aggregator tags an entry in
    /// [`SystemSnapshot::errors`] in that case rather than dropping
    /// the panel.
    pub temp_c: f32,
    /// `/proc/loadavg` 1 / 5 / 15-minute values.
    pub load_avg: [f32; 3],
}

/// Host memory panel — MiB-scaled to keep the JSON readable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemPanel {
    pub total_mib: u64,
    pub used_mib: u64,
    pub swap_used_mib: u64,
}

/// Per-GPU panel. `vendor` is a lowercase short tag (`"amd"`,
/// `"nvidia"`) so consumers can branch on it without a string-case
/// dance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuPanel {
    pub vendor: String,
    pub name: String,
    pub util_pct: u32,
    pub vram_used_mib: u64,
    pub vram_total_mib: u64,
    pub temp_c: f32,
    pub power_w: f32,
}

/// NPU panel. Mirrors the SPEC §4 example: a single accelerator per
/// host (`/dev/accel/accel0`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NpuPanel {
    /// Short tag (`"amd-xdna"`). Empty when no NPU is present.
    pub vendor: String,
    pub util_pct: u32,
    /// `true` when the device is in active power state. The popup
    /// shows a dim/lit icon based on this.
    pub active: bool,
    /// Firmware version string read from sysfs. May be empty on
    /// kernels that don't expose it.
    pub fw_version: String,
    pub power_w: f32,
    /// Live holders of `/dev/accel/accel0` as reported by `lsof` —
    /// usually `["sy-aiplane"]`, empty when the device is idle.
    pub holders: Vec<String>,
}

/// Per-interface network panel entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetIfacePanel {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Per-device disk panel entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskDevicePanel {
    pub name: String,
    pub reads: u64,
    pub writes: u64,
    pub io_in_progress: u64,
}

/// Aiplane panel — per-workload-kind queue / warm pool / latency
/// indexed by the [`crate::WorkloadKind`] string form. `BTreeMap` so
/// JSON key order is deterministic and the golden file stays stable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AiplanePanel {
    pub queue_depth: BTreeMap<String, u32>,
    pub warm: BTreeMap<String, u32>,
    pub latency_p99_ms: BTreeMap<String, f32>,
    pub errors_total: u64,
}

/// Knowledge plane panel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KnowledgePanel {
    pub collections: u32,
    pub docs_indexed: u64,
    pub embed_throughput_docs_per_s: f32,
    pub search_qps: f32,
}

/// Agent runner panel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentsPanel {
    pub running: u32,
    pub rss_total_mib: u64,
    pub policy_denials_recent: u32,
}

/// Power-governor panel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PowerPanel {
    pub current_arm: String,
    /// Fraction (0.0..=1.0) of the recent window spent in each arm.
    /// Sums to ~1.0 modulo float error. `BTreeMap` so the golden file
    /// has a stable key order.
    pub dwell_pct: BTreeMap<String, f32>,
    pub regret_cum: f32,
}

/// Supervisor panel: one row per plane.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SupervisorPanel {
    pub planes: Vec<PlanePanel>,
}

/// One supervised plane's state row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanePanel {
    pub name: String,
    pub state: String,
    pub restarts: u32,
}

/// One per-source error observed by the aggregator during a tick.
/// `plane` is `"host"` for sensor reads, otherwise the plane name
/// (`"aiplane"`, `"knowledge"`, etc.); `kind` is a short discriminator
/// (`"timeout"`, `"missing_socket"`, `"parse_error"`) so consumers can
/// group errors without parsing the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonError {
    pub plane: String,
    pub kind: String,
    pub message: String,
}

/// Shared latest-snapshot primitive (sy-mon ROADMAP Step 13).
///
/// The aggregator's 1 Hz tick publishes a fresh [`SystemSnapshot`]
/// every cycle; the `system.mon.snapshot` IPC handler reads the
/// most recent value lock-free; the `system.mon.subscribe` streamer
/// re-reads on every broadcast notification. `arc_swap::ArcSwap`
/// gives us "write-rarely / read-often" semantics with no contention
/// between producers and readers — the writer pays for an `Arc::new`
/// per tick and the readers pay for an atomic load.
///
/// Cloning a `LatestSnapshot` clones the inner `Arc` (shared
/// pointer), not the snapshot — the producer and every reader share
/// one storage cell.
#[derive(Debug, Default, Clone)]
pub struct LatestSnapshot {
    inner: Arc<ArcSwap<SystemSnapshot>>,
}

impl LatestSnapshot {
    /// Construct a `LatestSnapshot` seeded with [`SystemSnapshot::default`].
    /// The default carries the current [`SCHEMA_VERSION`] and empty
    /// panels so a `load()` before the first `store()` still produces
    /// a parseable wire shape.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(SystemSnapshot::default())),
        }
    }

    /// Publish a fresh snapshot. Cheap: one `Arc::new` plus an
    /// atomic pointer swap; the previous snapshot is dropped once
    /// every outstanding `load()` Arc-clone goes out of scope.
    pub fn store(&self, snap: SystemSnapshot) {
        self.inner.store(Arc::new(snap));
    }

    /// Atomically read the current snapshot. The returned `Arc` is a
    /// strong reference to the snapshot the producer published at
    /// `load()` time — a concurrent `store()` won't tear what the
    /// reader sees.
    pub fn load(&self) -> Arc<SystemSnapshot> {
        self.inner.load_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-rolled snapshot matching the SPEC §4 example data. The
    /// golden file is a literal copy of the spec example expanded so
    /// every panel struct sees at least one populated value. Built
    /// once and shared between the serialise and deserialise tests so
    /// the round-trip is exercised from a single source.
    fn spec_example_snapshot() -> SystemSnapshot {
        let mut queue_depth = BTreeMap::new();
        queue_depth.insert("embed".to_string(), 0);
        queue_depth.insert("rerank".to_string(), 2);
        let mut warm = BTreeMap::new();
        warm.insert("embed".to_string(), 1);
        warm.insert("rerank".to_string(), 1);
        let mut latency_p99_ms = BTreeMap::new();
        latency_p99_ms.insert("embed".to_string(), 18.4);
        latency_p99_ms.insert("rerank".to_string(), 41.0);
        let mut dwell_pct = BTreeMap::new();
        dwell_pct.insert("balanced".to_string(), 0.71);
        dwell_pct.insert("perf".to_string(), 0.18);
        dwell_pct.insert("save".to_string(), 0.11);

        SystemSnapshot {
            schema_version: SCHEMA_VERSION,
            captured_at_ms: 1_747_900_000_000,
            cpu: CpuPanel {
                per_core_util_pct: vec![12.3, 4.1],
                freq_mhz: vec![3800, 3800],
                temp_c: 58.2,
                load_avg: [1.42, 1.10, 0.95],
            },
            mem: MemPanel {
                total_mib: 32768,
                used_mib: 14210,
                swap_used_mib: 0,
            },
            gpu: vec![GpuPanel {
                vendor: "amd".to_string(),
                name: "Radeon 890M".to_string(),
                util_pct: 4,
                vram_used_mib: 512,
                vram_total_mib: 8192,
                temp_c: 49.0,
                power_w: 6.3,
            }],
            npu: NpuPanel {
                vendor: "amd-xdna".to_string(),
                util_pct: 73,
                active: true,
                fw_version: "1.5.10".to_string(),
                power_w: 4.2,
                holders: vec!["sy-aiplane".to_string()],
            },
            net: vec![NetIfacePanel {
                name: "wlan0".to_string(),
                rx_bytes: 1_234_567,
                tx_bytes: 89_012,
            }],
            disk: vec![DiskDevicePanel {
                name: "nvme0n1".to_string(),
                reads: 4321,
                writes: 765,
                io_in_progress: 0,
            }],
            aiplane: AiplanePanel {
                queue_depth,
                warm,
                latency_p99_ms,
                errors_total: 0,
            },
            knowledge: KnowledgePanel {
                collections: 4,
                docs_indexed: 17_402,
                embed_throughput_docs_per_s: 32.1,
                search_qps: 0.4,
            },
            agents: AgentsPanel {
                running: 2,
                rss_total_mib: 412,
                policy_denials_recent: 0,
            },
            power: PowerPanel {
                current_arm: "balanced".to_string(),
                dwell_pct,
                regret_cum: 0.034,
            },
            supervisor: SupervisorPanel {
                planes: vec![PlanePanel {
                    name: "aiplane".to_string(),
                    state: "active".to_string(),
                    restarts: 0,
                }],
            },
            errors: Vec::new(),
        }
    }

    /// The on-disk golden — checked in under
    /// `crates/sy-core/tests/snapshots/mon/spec-example.json`, ending
    /// in a trailing newline so future diffs stay readable.
    const GOLDEN_SPEC_EXAMPLE: &str = include_str!("../../tests/snapshots/mon/spec-example.json");

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, 1);
        assert_eq!(SystemSnapshot::default().schema_version, 1);
    }

    #[test]
    fn serialises_to_spec_example() {
        let snap = spec_example_snapshot();
        let serialised = serde_json::to_string_pretty(&snap)
            .expect("snapshot is plain data; serialising never fails")
            + "\n";
        assert_eq!(serialised, GOLDEN_SPEC_EXAMPLE);
    }

    #[test]
    fn round_trips_through_serde() {
        // The golden plus the typed example must agree both ways:
        // serialise the example to the golden, then deserialise the
        // golden back into a struct equal to the example. Guards
        // against a future field whose `Serialize` and `Deserialize`
        // impls drift apart.
        let snap = spec_example_snapshot();
        let parsed: SystemSnapshot =
            serde_json::from_str(GOLDEN_SPEC_EXAMPLE).expect("golden is valid JSON");
        assert_eq!(parsed, snap);
    }

    #[test]
    fn latest_snapshot_load_after_store_returns_stored_value() {
        // sy-mon Step 13: the aggregator's tick `store()`s a fresh
        // snapshot; the `system.mon.snapshot` IPC handler `load()`s it.
        // A regression where `store` didn't take or `load` returned a
        // stale Arc would silently freeze the dashboard at the first
        // tick. Pin the basic round-trip here.
        let latest = LatestSnapshot::new();
        let snap = SystemSnapshot {
            captured_at_ms: 42,
            ..SystemSnapshot::default()
        };
        latest.store(snap.clone());
        assert_eq!(latest.load().captured_at_ms, 42);
        assert_eq!(*latest.load(), snap);
    }

    #[test]
    fn latest_snapshot_default_seeds_schema_version() {
        // A reader that races ahead of the first `store()` must still
        // get a parseable snapshot — the wire shape promise in SPEC §4
        // is "always a `SystemSnapshot`, never an absent payload".
        // `LatestSnapshot::new()` seeds with the typed default, which
        // carries the current `SCHEMA_VERSION`.
        let latest = LatestSnapshot::new();
        let snap = latest.load();
        assert_eq!(snap.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn latest_snapshot_clones_share_storage() {
        // Cloning `LatestSnapshot` must produce two handles into the
        // same `ArcSwap` cell — the producer and the handlers each
        // hold their own clone but observe each other's writes.
        let producer = LatestSnapshot::new();
        let reader = producer.clone();
        let snap = SystemSnapshot {
            captured_at_ms: 7,
            ..SystemSnapshot::default()
        };
        producer.store(snap);
        assert_eq!(reader.load().captured_at_ms, 7);
    }
}
