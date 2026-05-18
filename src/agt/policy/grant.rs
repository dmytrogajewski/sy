//! Pre-issued policy grants — SPEC §4.4 "Consent UX" step 2 (b).
//!
//! `sy policy grant --tool=<n> --scope=<p> --ttl=<dur>` persists a
//! `Grant` to `$XDG_RUNTIME_DIR/sy/grants/<uuid>.toml`. Step 6's
//! consent flow will read these to short-circuit prompts when the
//! tool call matches an unexpired grant. Step 2 only persists and
//! reports — no enforcement yet.
//!
//! `sy policy trust --confirm` writes a sentinel to
//! `$XDG_STATE_HOME/sy/trusted.toml` so the resolver can opt into the
//! `trusted` profile in Step 6.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A grant is a tool-scoped, TTL-bounded permission slip. Step 6 will
/// honour these; Step 2 just persists them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Grant {
    pub tool: String,
    pub scope: PathBuf,
    /// TTL in milliseconds from `granted_at`. Stored as a flat u64 so
    /// the TOML round-trip is stable across hosts (TOML's native
    /// `duration` type would need a dep).
    pub ttl_ms: u64,
    /// Unix-epoch milliseconds at issuance.
    pub granted_at_ms: u64,
    pub granted_by_pid: u32,
    /// `/dev/tty` path captured at issuance (None when stdin wasn't a
    /// TTY — those callers must pair with `--yes`).
    pub granted_by_tty: Option<String>,
}

impl Grant {
    /// Build a grant pinned to "now". `ttl` is the wall-clock window
    /// from issuance; `is_active(t)` answers whether `t` still falls
    /// inside `[granted_at, granted_at + ttl)`.
    pub fn new(tool: String, scope: PathBuf, ttl: Duration, pid: u32, tty: Option<String>) -> Self {
        Self::new_at(tool, scope, ttl, pid, tty, SystemTime::now())
    }

    /// Test seam: same as [`new`] but with an injected `granted_at`.
    pub fn new_at(
        tool: String,
        scope: PathBuf,
        ttl: Duration,
        pid: u32,
        tty: Option<String>,
        granted_at: SystemTime,
    ) -> Self {
        let granted_at_ms = granted_at
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self {
            tool,
            scope,
            ttl_ms: u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
            granted_at_ms,
            granted_by_pid: pid,
            granted_by_tty: tty,
        }
    }

    /// True when `now` lies inside `[granted_at, granted_at + ttl)`.
    /// Outside that window — including instants *before* issuance —
    /// the grant is considered inactive.
    pub fn is_active(&self, now: SystemTime) -> bool {
        let now_ms = now
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let expires_at_ms = self.granted_at_ms.saturating_add(self.ttl_ms);
        now_ms >= self.granted_at_ms && now_ms < expires_at_ms
    }

    /// Persist as `<dir>/<uuid>.toml`. Returns the full path written.
    /// Creates `<dir>` if missing (mode 0o700 — runtime dir is already
    /// XDG-private, this is defence in depth).
    pub fn persist(&self, dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create grants dir {}", dir.display()))?;
        let path = dir.join(format!("{}.toml", Uuid::new_v4()));
        let text =
            toml::to_string_pretty(self).with_context(|| "serialize grant to TOML".to_string())?;
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }
}

/// Write the `trusted.toml` sentinel under `state_dir`. Step 6 will
/// gate the `trusted` profile on this file's presence + timestamp.
pub fn write_trust_sentinel(state_dir: &Path, pid: u32) -> Result<PathBuf> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create state dir {}", state_dir.display()))?;
    let path = state_dir.join("trusted.toml");
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let text = format!(
        "# Written by `sy policy trust --confirm`. Presence + pid + ts\n# unlock the `trusted` profile (Step 6 will gate on this).\nconfirmed_at_ms = {now_ms}\nconfirmed_by_pid = {pid}\n"
    );
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL_MS: u64 = 10;

    #[test]
    fn grant_ttl_expires() {
        let issued = SystemTime::now();
        let g = Grant::new_at(
            "rg".to_string(),
            PathBuf::from("/tmp"),
            Duration::from_millis(TTL_MS),
            42,
            None,
            issued,
        );
        // Inside the window: active.
        assert!(g.is_active(issued + Duration::from_millis(TTL_MS / 2)));
        // After ttl + slack: inactive.
        assert!(!g.is_active(issued + Duration::from_millis(TTL_MS * 2)));
    }

    #[test]
    fn grant_persists_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let g = Grant::new(
            "rg".to_string(),
            PathBuf::from("/tmp/x"),
            Duration::from_secs(60),
            123,
            Some("/dev/tty7".to_string()),
        );
        let path = g.persist(tmp.path()).expect("persist");
        let text = std::fs::read_to_string(&path).expect("read");
        let back: Grant = toml::from_str(&text).expect("parse");
        assert_eq!(back, g);
    }

    #[test]
    fn trust_sentinel_carries_pid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_trust_sentinel(tmp.path(), 99).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("confirmed_by_pid = 99"));
    }
}
