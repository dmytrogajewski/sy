//! Pre-boot repair pass for the qdrant storage tree.
//!
//! Qdrant performs atomic writes by creating sibling `.atomicwrite<rand>`
//! temp directories under each shard and renaming them into the segment
//! tree. A SIGKILL / OOM / power loss between `create()` and the final
//! fsync+rename of e.g. `vector_storage/vectors/config.json` leaves a
//! 0-byte file behind on disk. On the next boot the segment
//! loader's `serde_json::from_str` rejects it with
//! `EOF while parsing a value at line 1 column 0`, panicking qdrant
//! during `Collection::load`. With `BindsTo=sy-qdrant.service` cascading
//! into `sy-knowledge.service`, one corrupt file bricks the entire
//! knowledge plane (and the NPU embedder that lives inside it) until
//! a human notices and runs `find -size 0` by hand.
//!
//! This module owns the self-heal: walk every segment, quarantine any
//! that contain an empty / unparseable JSON file, sweep stale
//! `.atomicwrite*` leaks, and return a structured report. Called both
//! from `daemon::run()` (for the in-process supervisor path) and from
//! `sy knowledge repair-qdrant` (wired as `ExecStartPre=` on
//! `sy-qdrant.service` so the systemd-managed qdrant gets the same
//! pre-flight scrub). BUG-20260524-2203.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::Serialize;

/// Outcome of one `quarantine_corrupt_segments` pass. Stable for both
/// human-readable logging and the `--json` CLI surface.
#[derive(Debug, Default, Serialize)]
pub struct RepairReport {
    /// Segment directories moved out of the live tree, by their new
    /// (quarantined) absolute path.
    pub quarantined: Vec<QuarantinedSegment>,
    /// Stale `.atomicwrite*` temp directories removed during the sweep.
    pub swept_atomicwrite: usize,
    /// Shard directories that were inspected.
    pub shards_scanned: usize,
}

#[derive(Debug, Serialize)]
pub struct QuarantinedSegment {
    pub collection: String,
    pub shard: String,
    pub segment_id: String,
    /// Reason the segment was condemned (first failing JSON file +
    /// the parser error).
    pub reason: String,
    /// Absolute path of the segment's new home under
    /// `<shard>/segments.quarantine/<unix-ts>-<segment-id>/`.
    pub new_path: PathBuf,
}

/// Scan `<storage_dir>/collections/*/<shard>/segments/<uuid>/` and
/// quarantine any segment that contains at least one empty or
/// unparseable `*.json` file. Sweep stale `.atomicwrite*` directories
/// from each shard root in the same pass. Best-effort and idempotent:
/// missing collections/shards return an empty report, re-runs with
/// nothing to fix are zero-effect.
pub fn quarantine_corrupt_segments(storage_dir: &Path) -> Result<RepairReport> {
    let mut report = RepairReport::default();
    let collections = storage_dir.join("collections");
    if !collections.exists() {
        return Ok(report);
    }

    for coll_entry in read_dir(&collections)? {
        let coll_path = coll_entry.path();
        if !coll_path.is_dir() {
            continue;
        }
        let coll_name = file_name(&coll_path);

        for shard_entry in read_dir(&coll_path)? {
            let shard_path = shard_entry.path();
            if !shard_path.is_dir() {
                continue;
            }
            // Shards are numeric (`0`, `1`, ...). Skip anything else
            // (e.g. `snapshots`, `aliases`) — qdrant's own structure.
            let shard_name = file_name(&shard_path);
            if shard_name.parse::<u32>().is_err() {
                continue;
            }
            report.shards_scanned += 1;

            report.swept_atomicwrite += sweep_atomicwrite(&shard_path)?;

            let segments_dir = shard_path.join("segments");
            if !segments_dir.is_dir() {
                continue;
            }

            for seg_entry in read_dir(&segments_dir)? {
                let seg_path = seg_entry.path();
                if !seg_path.is_dir() {
                    continue;
                }
                let seg_id = file_name(&seg_path);
                // Don't recurse into our own quarantine outputs even
                // though we put them under <shard>/ not <segments>/.
                if let Some(reason) = first_bad_json(&seg_path) {
                    let new_path = quarantine_segment(&shard_path, &seg_path, &seg_id)?;
                    report.quarantined.push(QuarantinedSegment {
                        collection: coll_name.clone(),
                        shard: shard_name.clone(),
                        segment_id: seg_id,
                        reason,
                        new_path,
                    });
                }
            }
        }
    }

    Ok(report)
}

fn first_bad_json(seg_path: &Path) -> Option<String> {
    let mut hits: Vec<PathBuf> = Vec::new();
    collect_json_files(seg_path, &mut hits);
    // Deterministic order so log output is stable across runs.
    hits.sort();
    for json_path in hits {
        match validate_json(&json_path) {
            Ok(()) => {}
            Err(why) => {
                let rel = json_path
                    .strip_prefix(seg_path)
                    .unwrap_or(&json_path)
                    .display();
                return Some(format!("{rel}: {why}"));
            }
        }
    }
    None
}

fn collect_json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_json_files(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("json") {
            out.push(p);
        }
    }
}

fn validate_json(path: &Path) -> std::result::Result<(), String> {
    let meta = fs::metadata(path).map_err(|e| format!("stat: {e}"))?;
    if meta.len() == 0 {
        return Err("0-byte file".to_string());
    }
    let bytes = fs::read(path).map_err(|e| format!("read: {e}"))?;
    serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| format!("parse: {e}"))?;
    Ok(())
}

fn quarantine_segment(shard_path: &Path, seg_path: &Path, seg_id: &str) -> Result<PathBuf> {
    let q_root = shard_path.join("segments.quarantine");
    fs::create_dir_all(&q_root).with_context(|| format!("mkdir {}", q_root.display()))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let new_path = q_root.join(format!("{ts}-{seg_id}"));
    fs::rename(seg_path, &new_path)
        .with_context(|| format!("rename {} -> {}", seg_path.display(), new_path.display()))?;
    Ok(new_path)
}

fn sweep_atomicwrite(shard_path: &Path) -> Result<usize> {
    let mut removed = 0usize;
    let rd = match fs::read_dir(shard_path) {
        Ok(rd) => rd,
        Err(_) => return Ok(0),
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name = file_name(&p);
        if name.starts_with(".atomicwrite") {
            // Best-effort: a concurrent qdrant could be writing into one
            // of these, but the supervisor only calls us BEFORE spawning
            // qdrant, so there's no live writer. A failed remove is not
            // worth aborting startup over.
            if fs::remove_dir_all(&p).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn read_dir(p: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut out = Vec::new();
    for e in fs::read_dir(p).with_context(|| format!("read_dir {}", p.display()))? {
        out.push(e?);
    }
    Ok(out)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    /// Build a minimal but realistic qdrant layout:
    /// `<root>/collections/sy_knowledge/0/segments/<good>/...` (valid JSON)
    /// `<root>/collections/sy_knowledge/0/segments/<bad>/vector_storage/vectors/config.json`
    ///   is zero-byte (the live-host failure mode).
    /// Plus three `.atomicwriteXXX` leaks at the shard root.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let shard = tmp.path().join("collections/sy_knowledge/0");
        let good = shard.join("segments/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        let bad = shard.join("segments/bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");

        write(&good.join("segment.json"), r#"{"version":1}"#);
        write(
            &good.join("vector_storage/vectors/config.json"),
            r#"{"dim":768}"#,
        );

        write(&bad.join("segment.json"), r#"{"version":2}"#);
        // The exact corruption shape we observed on 2026-05-24.
        write(&bad.join("vector_storage/vectors/config.json"), "");

        // Three leaked atomicwrite temp dirs (399 in the real repro).
        for tag in ["X1aB", "Y2cD", "Z3eF"] {
            let p = shard.join(format!(".atomicwrite{tag}"));
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("scratch"), b"junk").unwrap();
        }

        let root = tmp.path().to_path_buf();
        (tmp, root)
    }

    #[test]
    fn quarantines_segment_with_empty_vector_config() {
        let (_tmp, root) = fixture();
        let report = quarantine_corrupt_segments(&root).unwrap();

        assert_eq!(report.quarantined.len(), 1);
        let q = &report.quarantined[0];
        assert_eq!(q.collection, "sy_knowledge");
        assert_eq!(q.shard, "0");
        assert!(q.segment_id.starts_with("bbbbbbbb"));
        assert!(q.reason.contains("vector_storage/vectors/config.json"));
        assert!(q.reason.contains("0-byte"));
        assert!(q.new_path.exists(), "quarantined dir should be present");

        let live = root.join("collections/sy_knowledge/0/segments");
        let names: Vec<String> = fs::read_dir(&live)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with("aaaaaaaa")),
            "good segment should remain live; got {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("bbbbbbbb")),
            "bad segment should have moved out of live; got {names:?}"
        );
    }

    #[test]
    fn leaves_healthy_segment_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let seg = tmp
            .path()
            .join("collections/sy_knowledge/0/segments/cccccccc-cccc-cccc-cccc-cccccccccccc");
        write(&seg.join("segment.json"), r#"{"version":1}"#);
        write(
            &seg.join("vector_storage/vectors/config.json"),
            r#"{"dim":768,"chunk_size_vectors":10922}"#,
        );
        write(
            &seg.join("payload_index/config.json"),
            r#"{"indexed_fields":{"tags":"keyword"}}"#,
        );

        let report = quarantine_corrupt_segments(tmp.path()).unwrap();
        assert!(report.quarantined.is_empty(), "{:?}", report.quarantined);
        assert!(seg.exists(), "healthy segment must not move");
    }

    #[test]
    fn sweeps_atomicwrite_leaks() {
        let (_tmp, root) = fixture();
        let shard = root.join("collections/sy_knowledge/0");
        let before: usize = fs::read_dir(&shard)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".atomicwrite"))
            .count();
        assert_eq!(before, 3);

        let report = quarantine_corrupt_segments(&root).unwrap();
        assert_eq!(report.swept_atomicwrite, 3);

        let after: usize = fs::read_dir(&shard)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".atomicwrite"))
            .count();
        assert_eq!(after, 0, "atomicwrite dirs should be gone");
    }

    #[test]
    fn second_run_is_noop() {
        let (_tmp, root) = fixture();
        let first = quarantine_corrupt_segments(&root).unwrap();
        assert_eq!(first.quarantined.len(), 1);
        assert_eq!(first.swept_atomicwrite, 3);

        let second = quarantine_corrupt_segments(&root).unwrap();
        assert!(second.quarantined.is_empty(), "{:?}", second.quarantined);
        assert_eq!(second.swept_atomicwrite, 0);
    }

    #[test]
    fn missing_storage_dir_returns_empty_report() {
        let tmp = tempfile::tempdir().unwrap();
        let report = quarantine_corrupt_segments(&tmp.path().join("does-not-exist")).unwrap();
        assert!(report.quarantined.is_empty());
        assert_eq!(report.swept_atomicwrite, 0);
        assert_eq!(report.shards_scanned, 0);
    }
}
