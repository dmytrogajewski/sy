//! Daemon-owned status snapshot at `$XDG_STATE_HOME/sy/aiplane/status.json`.
//!
//! The waybar applet (`sy knowledge waybar`) reads this file in <1 ms —
//! cheaper than any of the live probes (qdrant HTTP, manifest walk,
//! sy.toml parse). The daemon writes it atomically at lifecycle points
//! and fires `SIGRTMIN+11` at any waybar processes so the bar redraws
//! immediately rather than waiting for its poll interval.
//!
//! The shape is mostly knowledge-specific (the bar's tile is the
//! knowledge plane's UI), plus a per-workload health map under
//! `workloads` for the multi-workload aiplane.

use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    process::Command,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::registry::WorkloadHealth;

/// Waybar refresh signal used by `custom/sy-knowledge` (matches the
/// `"signal": 11` field in `configs/waybar/config.jsonc`). Sent as
/// `SIGRTMIN+11` via `pkill`.
pub const WAYBAR_SIGNAL_OFFSET: u32 = 11;

/// Status freshness limit. The applet treats files older than this as a
/// dead daemon and collapses the tile.
pub const FRESH_SECS: u64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    /// Wall-clock seconds since UNIX epoch when this snapshot was written.
    pub ts_unix: u64,
    pub daemon_running: bool,
    pub qdrant_ready: bool,
    pub schedule_secs: u64,
    /// Wall-clock seconds when the next scheduled pass is expected.
    pub next_run_unix: u64,
    pub sources_explicit: usize,
    pub sources_discover: usize,
    pub manifests_active: usize,
    pub manifests_disabled: usize,
    pub points: u64,
    /// True between `pass_started` and `pass_finished`.
    pub indexing: bool,
    /// True when the daemon's not firing scheduled / FS / IPC IndexNow
    /// passes (user paused indexing).
    #[serde(default)]
    pub paused: bool,
    /// True briefly while a Cancel is being honoured.
    #[serde(default)]
    pub cancelling: bool,
    /// `"vitisai"` | `"cpu"` | `"unloaded"`. Surfaces which
    /// ORT execution provider is wired up for embeddings.
    #[serde(default = "default_backend")]
    pub embed_backend: String,
    /// Human-readable label for the actual hardware doing the inference
    /// (e.g. `"AMD NPU on 9 HX 370"`, `"AMD Ryzen AI 9 HX 370 (CPU)"`).
    /// Filled in lazily on first embed.
    #[serde(default)]
    pub embed_hardware: String,
    /// Last finished pass: chunks/sec.
    #[serde(default)]
    pub last_throughput_chunks_per_s: Option<f32>,
    /// Configured cap from `[knowledge].cpu_max_percent` (None = uncapped).
    #[serde(default)]
    pub cpu_max_percent: Option<u8>,
    /// Last *finished* pass.
    #[serde(default)]
    pub last_index_at_unix: u64,
    #[serde(default)]
    pub last_index_ms: u64,
    #[serde(default)]
    pub last_index_indexed: usize,
    #[serde(default)]
    pub last_index_skipped: usize,
    #[serde(default)]
    pub last_index_deleted: usize,
    #[serde(default)]
    pub last_index_chunks: usize,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Per-workload health. Keyed by `WorkloadKind::as_str()`. Added
    /// May 2026 with the aiplane refactor — older snapshots without
    /// this field default to empty.
    #[serde(default)]
    pub workloads: HashMap<String, WorkloadHealth>,
}

fn default_backend() -> String {
    "unloaded".to_string()
}

pub fn status_path() -> Result<PathBuf> {
    Ok(root_dir()?.join("status.json"))
}

/// `$XDG_STATE_HOME/sy/aiplane/` (creates as needed). Older builds
/// wrote to `…/sy/knowledge/`; `daemon::run` migrates that directory
/// on startup, see `migrate_state_dir`.
pub fn root_dir() -> Result<PathBuf> {
    let base = if let Ok(d) = std::env::var("XDG_STATE_HOME") {
        if !d.is_empty() {
            PathBuf::from(d)
        } else {
            xdg_state_default()?
        }
    } else {
        xdg_state_default()?
    };
    let dir = base.join("sy").join("aiplane");
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    Ok(dir)
}

fn xdg_state_default() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local/state"))
}

/// Wall-clock seconds since the UNIX epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn load() -> Result<Status> {
    let p = status_path()?;
    let body = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let s: Status =
        serde_json::from_str(&body).with_context(|| format!("parse {}", p.display()))?;
    Ok(s)
}

/// Atomic write + best-effort waybar refresh.
pub fn save(status: &Status) -> Result<()> {
    let p = status_path()?;
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(status)?;
    {
        let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &p).with_context(|| format!("rename {}", p.display()))?;
    notify_waybar();
    Ok(())
}

/// Fire `SIGRTMIN+11` at every running waybar. Best-effort; silently
/// no-ops if `pkill` isn't on PATH or no waybar is up.
pub fn notify_waybar() {
    let signal = format!("-RTMIN+{WAYBAR_SIGNAL_OFFSET}");
    let _ = Command::new("pkill")
        .arg(&signal)
        .arg("waybar")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// True when the snapshot is younger than `FRESH_SECS`.
pub fn is_fresh(status: &Status) -> bool {
    let now = now_secs();
    now.saturating_sub(status.ts_unix) <= FRESH_SECS
}

/// One-shot migration on daemon startup. Older builds wrote state to
/// `$XDG_STATE_HOME/sy/knowledge/`; if that dir exists and the new
/// `sy/aiplane/` doesn't, atomically rename it. Idempotent.
pub fn migrate_state_dir() -> Result<()> {
    let base = if let Ok(d) = std::env::var("XDG_STATE_HOME") {
        if !d.is_empty() {
            PathBuf::from(d)
        } else {
            xdg_state_default()?
        }
    } else {
        xdg_state_default()?
    };
    let old = base.join("sy").join("knowledge");
    let new = base.join("sy").join("aiplane");
    if old.is_dir() && !new.exists() {
        std::fs::rename(&old, &new)
            .with_context(|| format!("migrate {} → {}", old.display(), new.display()))?;
        eprintln!(
            "sy aiplane: migrated state dir {} → {}",
            old.display(),
            new.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_json_roundtrip_with_workloads() {
        let mut workloads = HashMap::new();
        workloads.insert(
            "embed".to_string(),
            WorkloadHealth {
                loaded: true,
                last_call_unix: 1_700_000_000,
                ema_ms: 25.4,
                calls: 42,
                errors: 0,
                backend: "vitisai".to_string(),
            },
        );
        let s = Status {
            ts_unix: 1_700_000_001,
            daemon_running: true,
            qdrant_ready: true,
            schedule_secs: 1800,
            next_run_unix: 1_700_001_801,
            sources_explicit: 0,
            sources_discover: 3,
            manifests_active: 7,
            manifests_disabled: 0,
            points: 44_543,
            indexing: false,
            paused: false,
            cancelling: false,
            embed_backend: "vitisai".to_string(),
            embed_hardware: "AMD NPU on 9 HX 370".to_string(),
            last_throughput_chunks_per_s: Some(3.4),
            cpu_max_percent: None,
            last_index_at_unix: 1_700_000_000,
            last_index_ms: 192,
            last_index_indexed: 0,
            last_index_skipped: 21,
            last_index_deleted: 0,
            last_index_chunks: 0,
            last_error: None,
            workloads,
        };
        let json = serde_json::to_string(&s).unwrap();
        let s2: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(s2.workloads.get("embed").unwrap().calls, 42);
        assert_eq!(s2.embed_hardware, "AMD NPU on 9 HX 370");
    }

    #[test]
    fn old_snapshot_without_workloads_field_still_parses() {
        // Forward compatibility — pre-aiplane snapshots written by
        // sy-knowledge.service won't have a "workloads" key. They
        // must still parse.
        let legacy = r#"{
            "ts_unix": 1,
            "daemon_running": false,
            "qdrant_ready": false,
            "schedule_secs": 1800,
            "next_run_unix": 0,
            "sources_explicit": 0,
            "sources_discover": 0,
            "manifests_active": 0,
            "manifests_disabled": 0,
            "points": 0,
            "indexing": false
        }"#;
        let s: Status = serde_json::from_str(legacy).expect("legacy snapshot");
        assert!(s.workloads.is_empty());
    }

    #[test]
    fn is_fresh_within_window() {
        let mut s = test_status();
        s.ts_unix = now_secs();
        assert!(is_fresh(&s));
        s.ts_unix = now_secs().saturating_sub(FRESH_SECS + 1);
        assert!(!is_fresh(&s));
    }

    fn test_status() -> Status {
        Status {
            ts_unix: 0,
            daemon_running: false,
            qdrant_ready: false,
            schedule_secs: 0,
            next_run_unix: 0,
            sources_explicit: 0,
            sources_discover: 0,
            manifests_active: 0,
            manifests_disabled: 0,
            points: 0,
            indexing: false,
            paused: false,
            cancelling: false,
            embed_backend: String::new(),
            embed_hardware: String::new(),
            last_throughput_chunks_per_s: None,
            cpu_max_percent: None,
            last_index_at_unix: 0,
            last_index_ms: 0,
            last_index_indexed: 0,
            last_index_skipped: 0,
            last_index_deleted: 0,
            last_index_chunks: 0,
            last_error: None,
            workloads: HashMap::new(),
        }
    }
}
