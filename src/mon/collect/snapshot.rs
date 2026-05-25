//! Fold the host sample + per-plane scrape results into a
//! `SystemSnapshot` (SPEC §4 "SystemSnapshot JSON schema"). Pure
//! function — no I/O. The aggregator's tick performs the scrape calls
//! in parallel, gathers their results, then hands the bundle here.
//!
//! ## Mapping (Step 12)
//!
//! Step 12 focuses on the SPEC §4 "SystemSnapshot" field the tests
//! assert: `aiplane.queue_depth`. Other known
//! `crates/sy-core/src/metrics.rs::CORE_METRICS` names land in
//! `SystemSnapshot` panels they belong to as they get plumbed; samples
//! whose target panel has no field yet are dropped silently (per Step
//! 12 spec: unrecognised samples are NOT tagged as errors).
//!
//! | Prometheus name                | Snapshot field                  |
//! |--------------------------------|---------------------------------|
//! | `sy_queue_depth{kind=...}`     | `aiplane.queue_depth[kind]`     |
//! | `sy_models_warm{kind=...}`     | (Step 12 follow-up; ignored)    |
//! | `sy_workload_completed_total`  | (Step 12 follow-up; ignored)    |
//! | `sy_workload_errors_total`     | `aiplane.errors_total` (sum)    |
//! | `sy_workload_latency_seconds`  | (histogram; Step 12 follow-up)  |
//! | `sy_policy_denials_total`      | (Step 12 follow-up; ignored)    |
//! | `sy_ipc_errors_total`          | (Step 12 follow-up; ignored)    |
//! | `sy_npu_temp_celsius`          | (Step 12 follow-up; ignored)    |

use anyhow::Error;
use sy_core::mon::snapshot::{MonError, SystemSnapshot};

use super::sample::HostSample;
use super::scrape::PlaneMetrics;

/// One plane scrape result, paired with the canonical plane name so
/// the fold can record an error under the right key when the scrape
/// failed before `PlaneMetrics` was constructed.
pub type PlaneScrape = (String, Result<PlaneMetrics, Error>);

/// `MonError.kind` value when the UDS connect / HTTP exchange failed
/// (ENOENT on the socket, the plane daemon being down, garbled body).
pub const KIND_SCRAPE_FAILED: &str = "scrape_failed";

/// Build a `SystemSnapshot` from a host sample and a slice of plane
/// scrape results. Pure function — no I/O, no time reads.
///
/// `errors` is the tick-level accumulator the host phase already
/// populated (host sampler timeouts / panics); fold appends any
/// per-plane scrape failures and returns the merged set in
/// `SystemSnapshot.errors`.
pub fn fold_into_snapshot(
    host: HostSample,
    planes: &[PlaneScrape],
    errors: &mut Vec<MonError>,
) -> SystemSnapshot {
    let mut snap = SystemSnapshot::default();
    apply_host(&mut snap, &host);
    for (plane, result) in planes {
        match result {
            Ok(metrics) => apply_plane(&mut snap, metrics),
            Err(e) => errors.push(MonError {
                plane: plane.clone(),
                kind: KIND_SCRAPE_FAILED.to_string(),
                message: format!("{e:#}"),
            }),
        }
    }
    snap.errors = errors.clone();
    snap
}

/// Project the host sample into the snapshot's host panels.
/// `project_row` covers the ring-buffer view; this keeps the snapshot
/// wire shape independent of the ring layout per
/// `crates/sy-core/src/mon/snapshot.rs` doc comment.
fn apply_host(snap: &mut SystemSnapshot, host: &HostSample) {
    if let Some(cpu) = &host.cpu {
        snap.cpu.per_core_util_pct = cpu.per_core_util_pct.clone();
        snap.cpu.freq_mhz = cpu.freq_mhz.clone();
        if let Some(t) = cpu.temp_c {
            snap.cpu.temp_c = t;
        }
    }
    if let Some(mem) = &host.mem {
        snap.mem.total_mib = mem.total_mib;
        snap.mem.used_mib = mem.used_mib;
        snap.mem.swap_used_mib = mem.swap_used_mib;
    }
    if let Some(load) = &host.load {
        snap.cpu.load_avg = [load.one, load.five, load.fifteen];
    }
    if let Some(net) = &host.net {
        snap.net = net
            .interfaces
            .iter()
            .map(|i| sy_core::mon::snapshot::NetIfacePanel {
                name: i.name.clone(),
                rx_bytes: i.rx_bytes,
                tx_bytes: i.tx_bytes,
            })
            .collect();
    }
    if let Some(disk) = &host.disk {
        snap.disk = disk
            .devices
            .iter()
            .map(|d| sy_core::mon::snapshot::DiskDevicePanel {
                name: d.name.clone(),
                reads: d.reads,
                writes: d.writes,
                io_in_progress: d.io_in_progress,
            })
            .collect();
    }
    // Merge AMD + NVIDIA GPUs into one panel list (vendor disambiguates).
    snap.gpu.clear();
    for amd in &host.gpu_amd.cards {
        snap.gpu.push(sy_core::mon::snapshot::GpuPanel {
            vendor: "amd".into(),
            name: amd.name.clone(),
            util_pct: amd.busy_pct.map(u32::from).unwrap_or(0),
            vram_used_mib: amd.vram_used_bytes.unwrap_or(0) / (1024 * 1024),
            vram_total_mib: amd.vram_total_bytes.unwrap_or(0) / (1024 * 1024),
            temp_c: amd.temp_c.unwrap_or(0.0),
            power_w: amd.power_w.unwrap_or(0.0),
        });
    }
    for nv in &host.gpu_nvidia.gpus {
        snap.gpu.push(sy_core::mon::snapshot::GpuPanel {
            vendor: "nvidia".into(),
            name: nv.name.clone(),
            util_pct: nv.util_pct.map(u32::from).unwrap_or(0),
            vram_used_mib: nv.vram_used_mib.unwrap_or(0),
            vram_total_mib: nv.vram_total_mib.unwrap_or(0),
            temp_c: nv.temp_c.unwrap_or(0.0),
            power_w: nv.power_w.unwrap_or(0.0),
        });
    }
    if let Some(npu) = &host.npu {
        snap.npu = sy_core::mon::snapshot::NpuPanel {
            vendor: "amd-xdna".into(),
            util_pct: npu.util_pct,
            active: npu.active,
            fw_version: npu.fw_version.clone().unwrap_or_default(),
            power_w: npu.power_w.unwrap_or(0.0),
            holders: npu.holders.clone(),
        };
    }
}

/// Apply one plane's parsed samples to the snapshot. Step 12 surfaces
/// `sy_queue_depth` into `aiplane.queue_depth`; unrecognised samples
/// are dropped silently per SPEC ("samples not in CORE_METRICS are
/// dropped silently. Don't tag them as errors").
fn apply_plane(snap: &mut SystemSnapshot, metrics: &PlaneMetrics) {
    if metrics.plane != "aiplane" {
        // Other planes don't surface aiplane fields. Step 20 wires the
        // remaining producers (knowledge / agt / supervisor / …); when
        // those land, the matching projections grow here.
        return;
    }
    for sample in &metrics.samples {
        if sample.metric == "sy_queue_depth" {
            if let Some(kind) = sample.labels.get("kind") {
                if let prometheus_parse::Value::Gauge(v) = &sample.value {
                    let clamped = v.max(0.0).min(u32::MAX as f64) as u32;
                    snap.aiplane.queue_depth.insert(kind.to_string(), clamped);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::collect::scrape::PlaneMetrics;

    /// Same canned exposition the scrape test serves over a fake UDS
    /// — driving the fold from the literal fixture keeps the
    /// producer-format and consumer-format coupled to one source.
    const AIPLANE_FIXTURE: &str =
        include_str!("../../../tests/fixtures/mon/prom/aiplane/metrics.txt");
    const KNOWLEDGE_FIXTURE: &str =
        include_str!("../../../tests/fixtures/mon/prom/knowledge/metrics.txt");

    /// Helper: parse a fixture into a `PlaneMetrics` for the fold tests.
    /// Mirrors `scrape::parse_body` but stays inside the test module so
    /// the scraper's I/O surface doesn't need to be re-exported.
    fn fixture_metrics(plane: &str, body: &str) -> PlaneMetrics {
        let lines = body
            .lines()
            .map(|l| std::io::Result::Ok(l.to_string()))
            .collect::<Vec<_>>();
        let scrape = prometheus_parse::Scrape::parse(lines.into_iter()).expect("fixture parses");
        PlaneMetrics {
            plane: plane.to_string(),
            samples: scrape.samples,
        }
    }

    /// SPEC §4 SystemSnapshot fold: two plane scrapes resolve into the
    /// `aiplane.queue_depth` map. The aiplane fixture sets
    /// `sy_queue_depth{kind="embed"} 2`; the knowledge fixture's
    /// `sy_queue_depth` is ignored (Step 12 only surfaces aiplane).
    #[test]
    fn fold_two_planes_into_snapshot() {
        let aiplane = Ok(fixture_metrics("aiplane", AIPLANE_FIXTURE));
        let knowledge = Ok(fixture_metrics("knowledge", KNOWLEDGE_FIXTURE));
        let mut errors = Vec::new();
        let snap = fold_into_snapshot(
            HostSample::default(),
            &[
                ("aiplane".to_string(), aiplane),
                ("knowledge".to_string(), knowledge),
            ],
            &mut errors,
        );
        assert!(
            snap.errors.is_empty(),
            "no scrape errors: {:?}",
            snap.errors
        );
        assert_eq!(
            snap.aiplane.queue_depth.get("embed").copied(),
            Some(2),
            "expected aiplane.queue_depth[embed] == 2, got {:?}",
            snap.aiplane.queue_depth
        );
    }

    /// SPEC §4 Reliability: a missing plane socket lands in `errors[]`
    /// with `kind == "scrape_failed"`, and the affected panel stays at
    /// its zero shape (empty `queue_depth`). The host panel is
    /// untouched because the host sampler is on a different code path.
    #[test]
    fn missing_socket_yields_zero_with_error() {
        let failed: Result<PlaneMetrics, Error> =
            Err(anyhow::anyhow!("ENOENT: aiplane metrics.sock"));
        let mut errors = Vec::new();
        let snap = fold_into_snapshot(
            HostSample::default(),
            &[("aiplane".to_string(), failed)],
            &mut errors,
        );
        assert!(
            snap.aiplane.queue_depth.is_empty(),
            "missing socket must leave queue_depth empty: {:?}",
            snap.aiplane.queue_depth
        );
        let err = snap
            .errors
            .iter()
            .find(|e| e.plane == "aiplane")
            .expect("missing-socket error must surface in errors[]");
        assert_eq!(err.kind, KIND_SCRAPE_FAILED);
        assert!(
            err.message.contains("ENOENT"),
            "scrape error message should preserve the cause: {}",
            err.message
        );
    }
}
