//! cgroup-v2 ancestry watcher (SPEC §2 "12-signal panel": the
//! cgroup-ancestry channel). Walks `/proc/<pid>/cgroup` for every
//! pid procfs surfaces and matches the ancestry path against an
//! allow-list of substrings — the canonical entries are app-launcher
//! slice names ("firefox", "vscode", "alacritty") that the
//! forecaster correlates with activity classes.
//!
//! The parser is a free function so tests can feed synthetic cgroup
//! lines without needing a real `/proc`. The channel side wraps it in
//! a `procfs::process::all_processes` walk and dedupes against the
//! pid-set seen on the previous tick — only **new** processes fire
//! `ProcessFromAncestor`.

use std::collections::HashSet;

use procfs::process::all_processes;

use super::{IntentChannel, IntentEvent};

/// cgroup-v2 lines start with `0::/` — older `0::` + `1::` (cgroup-v1
/// hybrid) lines exist but the desktop slice we care about is always
/// the v2 one. We match against the full line so the parser stays
/// agnostic to which slice index it landed on.
const CGROUP_V2_PREFIX: &str = "0::";

/// Pure-fn matcher: given one line from `/proc/<pid>/cgroup` and an
/// allow-list of substrings, return the first matching entry. Match
/// is case-insensitive substring of the path — robust against
/// systemd's `app-glib-firefox-12345.scope` / `app-firefox.scope` /
/// `firefox.scope` shapes.
pub fn matches_ancestor(cgroup_line: &str, allow_list: &[&str]) -> Option<String> {
    if !cgroup_line.starts_with(CGROUP_V2_PREFIX) {
        return None;
    }
    let line_lc = cgroup_line.to_lowercase();
    for needle in allow_list {
        if !needle.is_empty() && line_lc.contains(&needle.to_lowercase()) {
            return Some((*needle).to_string());
        }
    }
    None
}

/// Stateful channel: holds the allow-list + the pid-set from the
/// previous `poll()` so a long-lived Firefox doesn't refire on every
/// tick. `procfs` errors degrade silently — the channel keeps running
/// (matches `LogindChannel::BusUnreachable` shape).
pub struct CgroupAncestryChannel {
    allow_list: Vec<String>,
    seen_pids: HashSet<i32>,
}

impl CgroupAncestryChannel {
    pub fn new<S: Into<String>>(allow_list: impl IntoIterator<Item = S>) -> Self {
        Self {
            allow_list: allow_list.into_iter().map(Into::into).collect(),
            seen_pids: HashSet::new(),
        }
    }

    fn allow_refs(&self) -> Vec<&str> {
        self.allow_list.iter().map(String::as_str).collect()
    }
}

impl IntentChannel for CgroupAncestryChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        let allow = self.allow_refs();
        let procs = all_processes().ok()?;
        let mut current: HashSet<i32> = HashSet::new();
        let mut hit: Option<IntentEvent> = None;
        for p in procs.flatten() {
            let pid = p.pid;
            current.insert(pid);
            if self.seen_pids.contains(&pid) || hit.is_some() {
                continue;
            }
            let cgroups = match p.cgroups() {
                Ok(c) => c,
                Err(_) => continue,
            };
            for entry in &cgroups.0 {
                // procfs reformats the line; we reconstruct the
                // canonical form so `matches_ancestor` works on the
                // same shape tests feed it.
                let line = format!(
                    "{}::{}:{}",
                    entry.hierarchy,
                    entry.controllers.join(","),
                    entry.pathname,
                );
                if let Some(name) = matches_ancestor(&line, &allow) {
                    hit = Some(IntentEvent::ProcessFromAncestor { name });
                    break;
                }
            }
        }
        self.seen_pids = current;
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical systemd-on-Fedora shape for a Firefox flatpak
    /// scope, copied from a live `/proc/<pid>/cgroup`. The parser
    /// must pick out `"firefox"` from the allow-list.
    #[test]
    fn detects_ancestor_match() {
        let line = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-glib-firefox-12345.scope";
        let allow = ["firefox"];
        assert_eq!(matches_ancestor(line, &allow), Some("firefox".to_string()));
    }

    /// cgroup-v1 hybrid lines (`1::`, `2::`) — older systems, NOT the
    /// signal we want. Parser must drop them.
    #[test]
    fn ignores_non_v2_lines() {
        let line = "1::/user.slice/user-1000.slice/app.slice/firefox.scope";
        assert_eq!(matches_ancestor(line, &["firefox"]), None);
    }

    /// Empty allow-list ⇒ never matches. Guards against
    /// `line.contains("")` always being `true`.
    #[test]
    fn empty_allow_list_never_matches() {
        let line = "0::/user.slice/firefox.scope";
        assert_eq!(matches_ancestor(line, &[]), None);
        assert_eq!(matches_ancestor(line, &[""]), None);
    }

    /// Case-insensitive: a `.scope` named `Firefox.scope` (Snap, some
    /// distros) still matches the lowercase allow-list entry.
    #[test]
    fn case_insensitive_match() {
        let line = "0::/user.slice/app-Firefox.scope";
        assert_eq!(matches_ancestor(line, &["firefox"]), Some("firefox".into()));
    }
}
