//! systemd-logind inhibitor watcher (SPEC §2 "12-signal panel": logind
//! is the de-facto Linux "user is in a call" signal — Teams / Slack /
//! Discord all grab `Inhibit("idle")` for the duration of the call;
//! Zoom adds `com.zoom.HotKeyService` on the session bus, caught as a
//! fallback by Step 7's ScreenCast watcher).
//!
//! Mechanism per <https://systemd.io/INHIBITOR_LOCKS/>:
//! 1. Connect to the system bus and proxy
//!    `org.freedesktop.login1.Manager`.
//! 2. Call `ListInhibitors()` — returns `Vec<(what, who, why, mode,
//!    uid, pid)>`.
//! 3. For each entry, run `classify_inhibitor` against the configured
//!    `Whitelist`. The classifier fires when `what` contains `"idle"`
//!    and `who` substring-matches one of the whitelist entries
//!    case-insensitively (casing varies by app).
//!
//! Step 5 ships the pure-fn classifier + a `zbus::blocking`-backed
//! `LogindChannel` that drains a single pending event per `poll()`
//! call. Background subscription to `PrepareForSleep` /
//! `PropertyChanged` lands together with the daemon loop in Step 10 —
//! today, the channel re-polls `ListInhibitors` on every `poll()`
//! tick, which is fine for the 1 Hz daemon cadence.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::{IntentChannel, IntentEvent};

/// systemd-logind's well-known bus name.
const LOGIND_BUS: &str = "org.freedesktop.login1";
/// Manager object path on the system bus.
const LOGIND_PATH: &str = "/org/freedesktop/login1";
/// Manager interface that exposes `ListInhibitors()`.
const LOGIND_IFACE: &str = "org.freedesktop.login1.Manager";

/// The `what` field of an inhibitor lock is a colon-separated set
/// (e.g. `"idle:sleep:handle-power-key"`). We fire on any entry that
/// includes `"idle"` — see <https://systemd.io/INHIBITOR_LOCKS/>.
const WHAT_IDLE: &str = "idle";

/// Schema for `configs/sy/intent_whitelist.toml`. Only the `[call]`
/// table is populated in Step 5; Step 7's media / portal / notify
/// channels will append their own tables here.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Whitelist {
    #[serde(default)]
    pub call: CallWhitelist,
}

/// Whitelist of `Who` substrings that should fire `CallActive`. The
/// canonical Step 5 set (SPEC §2) is "Microsoft Teams", "Slack",
/// "zoom", "Discord".
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct CallWhitelist {
    #[serde(default)]
    pub who: Vec<String>,
}

impl Whitelist {
    /// Load from a TOML file. Missing file ⇒ empty whitelist; parse
    /// errors propagate so the operator notices a typo'd config (the
    /// daemon-level fallback is "construct a default Whitelist and
    /// log a warning" — see `LogindChannel::new`).
    pub fn load(path: &Path) -> Result<Self, LogindError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|_| LogindError::WhitelistIo)?;
        toml::from_str::<Self>(&text).map_err(|_| LogindError::WhitelistParse)
    }
}

/// Errors the `LogindChannel` reports. None of these are fatal — the
/// daemon treats every variant as "channel disabled, keep running",
/// mirroring `psi::PsiError::Unavailable`.
#[derive(Debug)]
pub enum LogindError {
    /// `zbus::blocking::Connection::system()` failed. Common on CI
    /// runners, containers without `/var/run/dbus`, and the no-bus
    /// integration-test environment.
    BusUnreachable,
    /// `ListInhibitors()` call returned an error (logind not running,
    /// or the proxy creation failed).
    ListInhibitorsFailed,
    /// `read_to_string` on the whitelist path failed (permission,
    /// I/O error).
    WhitelistIo,
    /// The whitelist TOML failed to deserialise.
    WhitelistParse,
}

impl std::fmt::Display for LogindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogindError::BusUnreachable => write!(f, "system bus unreachable"),
            LogindError::ListInhibitorsFailed => {
                write!(f, "logind ListInhibitors call failed")
            }
            LogindError::WhitelistIo => write!(f, "intent whitelist read failed"),
            LogindError::WhitelistParse => write!(f, "intent whitelist parse failed"),
        }
    }
}

impl std::error::Error for LogindError {}

/// Pure-fn classifier. Returns `Some(CallActive { who })` iff:
/// - `what` contains `"idle"` (a process holding `sleep` +
///   `handle-power-key` + `idle` simultaneously still fires; we only
///   care about the idle bit).
/// - `who` substring-matches at least one whitelist entry,
///   case-insensitively (casing varies per app: Teams sends
///   "Microsoft Teams", Slack sends "Slack", Zoom sends "zoom").
pub fn classify_inhibitor(what: &str, who: &str, whitelist: &Whitelist) -> Option<IntentEvent> {
    if !what.split(':').any(|tag| tag == WHAT_IDLE) {
        return None;
    }
    let who_lc = who.to_lowercase();
    for needle in &whitelist.call.who {
        if !needle.is_empty() && who_lc.contains(&needle.to_lowercase()) {
            return Some(IntentEvent::CallActive {
                who: who.to_string(),
            });
        }
    }
    None
}

/// Stateful logind inhibitor channel. Holds a `zbus::blocking`
/// connection + the loaded whitelist; `poll()` calls `ListInhibitors`,
/// classifies each entry, and returns the first new `CallActive`
/// event. The "background subscriber" plumbing (PrepareForSleep +
/// PropertyChanged) lives in Step 10 — at 1 Hz daemon cadence,
/// re-polling on the tick is sufficient and adds no observable
/// latency vs an epoll-driven subscription.
pub struct LogindChannel {
    whitelist: Whitelist,
    proxy: zbus::blocking::Proxy<'static>,
    /// Tracks the `who` of the last emitted event so a sustained
    /// inhibitor doesn't re-fire `CallActive` every tick. Cleared
    /// when the inhibitor disappears.
    last_emitted: Arc<Mutex<Option<String>>>,
}

impl LogindChannel {
    /// Connect to the system bus and load `whitelist_path`. Returns
    /// `Err(LogindError::BusUnreachable)` when the bus is absent (CI,
    /// minimal containers) — the daemon keeps running with the
    /// channel disabled (no-snowflake degradation, matches
    /// `PsiChannel::new`).
    pub fn new(whitelist_path: &Path) -> Result<Self, LogindError> {
        let whitelist = Whitelist::load(whitelist_path).unwrap_or_default();
        let conn = zbus::blocking::Connection::system().map_err(|_| LogindError::BusUnreachable)?;
        let proxy = zbus::blocking::Proxy::new(&conn, LOGIND_BUS, LOGIND_PATH, LOGIND_IFACE)
            .map_err(|_| LogindError::BusUnreachable)?;
        Ok(Self {
            whitelist,
            proxy,
            last_emitted: Arc::new(Mutex::new(None)),
        })
    }

    /// One ListInhibitors round-trip → first whitelist match.
    fn poll_bus(&self) -> Result<Option<IntentEvent>, LogindError> {
        let entries: Vec<(String, String, String, String, u32, u32)> = self
            .proxy
            .call("ListInhibitors", &())
            .map_err(|_| LogindError::ListInhibitorsFailed)?;
        for (what, who, _why, _mode, _uid, _pid) in &entries {
            if let Some(ev) = classify_inhibitor(what, who, &self.whitelist) {
                return Ok(Some(ev));
            }
        }
        Ok(None)
    }
}

impl IntentChannel for LogindChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        let next = self.poll_bus().ok().flatten();
        let mut slot = self.last_emitted.lock().ok()?;
        match (&next, slot.as_deref()) {
            // No active inhibitor → reset state, emit nothing.
            (None, _) => {
                *slot = None;
                None
            }
            // Already reported this exact `who` last tick → suppress.
            (Some(IntentEvent::CallActive { who }), Some(prev)) if who == prev => None,
            // New (or changed) inhibitor → emit + remember.
            (Some(IntentEvent::CallActive { who }), _) => {
                *slot = Some(who.clone());
                Some(IntentEvent::CallActive { who: who.clone() })
            }
            // Defensive: classifier never returns non-CallActive events.
            (Some(other), _) => Some(other.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four canonical Step 5 whitelist entries (SPEC §2). Keeps
    /// the test independent of `configs/sy/intent_whitelist.toml`
    /// file shape — that file is regression-tested separately by the
    /// loader.
    fn canonical_whitelist() -> Whitelist {
        Whitelist {
            call: CallWhitelist {
                who: vec![
                    "Microsoft Teams".into(),
                    "Slack".into(),
                    "zoom".into(),
                    "Discord".into(),
                ],
            },
        }
    }

    /// `Inhibit("idle")` from "Microsoft Teams" → fires `CallActive`.
    /// Covers the exact `Who` string Teams Linux client sends, per
    /// SPEC §2's reference.
    #[test]
    fn whitelist_matches_teams() {
        let wl = canonical_whitelist();
        let ev = classify_inhibitor("idle", "Microsoft Teams", &wl);
        assert_eq!(
            ev,
            Some(IntentEvent::CallActive {
                who: "Microsoft Teams".into()
            })
        );
    }

    /// Casing varies per app: Zoom Linux client sends lowercase
    /// "zoom", Slack sends "Slack", Discord sometimes sends "discord"
    /// (snap build) and sometimes "Discord" (flatpak). All four must
    /// match regardless of casing.
    #[test]
    fn whitelist_matches_other_apps_case_insensitively() {
        let wl = canonical_whitelist();
        for who in ["zoom", "Slack", "discord", "DISCORD"] {
            let ev = classify_inhibitor("idle", who, &wl);
            assert!(
                matches!(ev, Some(IntentEvent::CallActive { .. })),
                "expected CallActive for who={who:?}, got {ev:?}"
            );
        }
    }

    /// Compound `what` field: systemd common-case is
    /// `"idle:sleep:handle-power-key"` — Teams typically grabs all
    /// three. Must still fire because `idle` is present.
    #[test]
    fn whitelist_matches_compound_what() {
        let wl = canonical_whitelist();
        let ev = classify_inhibitor("idle:sleep:handle-power-key", "Microsoft Teams", &wl);
        assert!(matches!(ev, Some(IntentEvent::CallActive { .. })));
    }

    /// `systemd-update.service` holds `Inhibit("sleep")` during
    /// pending OS updates. Different mechanism, different signal —
    /// must NOT fire `CallActive` even if the `Who` happens to
    /// substring-match.
    #[test]
    fn ignores_non_idle_inhibitors() {
        let wl = canonical_whitelist();
        let ev = classify_inhibitor("sleep", "Microsoft Teams", &wl);
        assert_eq!(ev, None);
    }

    /// `sleep:handle-power-key` (no `idle` tag) from a whitelisted
    /// `Who` also gets dropped — the classifier keys on the `what`
    /// field, not just the app name.
    #[test]
    fn ignores_sleep_only_compound_what() {
        let wl = canonical_whitelist();
        let ev = classify_inhibitor("sleep:handle-power-key", "Slack", &wl);
        assert_eq!(ev, None);
    }

    /// A whitelisted `what=idle` from an unknown app (e.g. Firefox
    /// holding the lock during a video) must NOT fire — Step 5 is
    /// strictly the conferencing-app channel.
    #[test]
    fn ignores_unknown_who() {
        let wl = canonical_whitelist();
        let ev = classify_inhibitor("idle", "Firefox", &wl);
        assert_eq!(ev, None);
    }

    /// Empty whitelist (e.g. the operator deleted the `[call]`
    /// stanza) is well-defined: zero matches, every event suppressed.
    #[test]
    fn empty_whitelist_never_matches() {
        let wl = Whitelist::default();
        let ev = classify_inhibitor("idle", "Microsoft Teams", &wl);
        assert_eq!(ev, None);
    }

    /// An empty `Who` string from a misbehaving caller must not
    /// match an empty whitelist entry (guards against
    /// `who.contains("")` always being `true`).
    #[test]
    fn empty_who_does_not_match() {
        let wl = canonical_whitelist();
        let ev = classify_inhibitor("idle", "", &wl);
        assert_eq!(ev, None);
    }

    /// The shipped `configs/sy/intent_whitelist.toml` parses cleanly
    /// and yields the four canonical names — regression-tests the
    /// on-disk file shape against the Step 5 contract.
    #[test]
    fn shipped_whitelist_loads() {
        // Walk up from the test crate dir to the repo root, then to
        // the in-tree config. The test harness runs from the package
        // dir, so `configs/sy/...` is the relative path.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("configs/sy/intent_whitelist.toml");
        let wl = Whitelist::load(&path).expect("shipped whitelist parses");
        assert!(
            wl.call.who.iter().any(|s| s == "Microsoft Teams"),
            "shipped whitelist must contain Microsoft Teams; got {:?}",
            wl.call.who
        );
        assert!(wl.call.who.iter().any(|s| s == "Slack"));
        assert!(wl.call.who.iter().any(|s| s == "zoom"));
        assert!(wl.call.who.iter().any(|s| s == "Discord"));
    }

    /// Missing file ⇒ default (empty) whitelist, no error. Daemon
    /// keeps running.
    #[test]
    fn missing_whitelist_yields_default() {
        let wl = Whitelist::load(Path::new("/nonexistent/intent_whitelist.toml"))
            .expect("missing file is non-fatal");
        assert!(wl.call.who.is_empty());
    }
}
