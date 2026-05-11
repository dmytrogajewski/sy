//! Daemon-owned status snapshot at `$XDG_STATE_HOME/sy/knowledge/status.json`.
//!
//! The waybar applet (`sy knowledge waybar`) reads this file in <1 ms —
//! cheaper than any of the live probes (qdrant HTTP, manifest walk,
//! sy.toml parse). The daemon writes it atomically at lifecycle points
//! and fires `SIGRTMIN+11` at any waybar processes so the bar redraws
//! immediately rather than waiting for its poll interval.

use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
    process::Command,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::state;

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
    /// `"cuda"` | `"cpu"` | `"unloaded"`. Surfaces whether GPU is engaged.
    #[serde(default = "default_backend")]
    pub embed_backend: String,
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
}

fn default_backend() -> String {
    "unloaded".to_string()
}

pub fn status_path() -> Result<PathBuf> {
    Ok(state::root_dir()?.join("status.json"))
}

pub fn load() -> Result<Status> {
    let p = status_path()?;
    let body = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let s: Status = serde_json::from_str(&body)
        .with_context(|| format!("parse {}", p.display()))?;
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
    let now = state::now_secs();
    now.saturating_sub(status.ts_unix) <= FRESH_SECS
}
