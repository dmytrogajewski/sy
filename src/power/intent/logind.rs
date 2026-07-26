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
//! `LogindChannel`. `poll()` reports the **level**, not an edge: it
//! re-runs `ListInhibitors` every tick and returns `CallActive`
//! whenever a whitelisted idle-inhibitor is currently held, and `None`
//! the moment it is released. `call_active` in the snapshot must track
//! the lock being held for its full duration (BUG-20260712-1200) — an
//! earlier `last_emitted` de-dup collapsed the signal to a single
//! grab-edge tick, so telemetry saw `call_active` flip false while the
//! inhibitor was still held. The `InhibitorSource` seam abstracts the
//! bus call so the level semantics are testable without a live system
//! bus. Background subscription to `PrepareForSleep` / `PropertyChanged`
//! is deferred; at 1 Hz re-polling adds no observable latency.

use std::path::Path;

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

/// One row of `ListInhibitors()`: `(what, who, why, mode, uid, pid)`.
type InhibitorEntry = (String, String, String, String, u32, u32);

/// Source of the currently-held inhibitor list. Abstracted so
/// `LogindChannel::poll` is a pure level read — testable without a
/// live system bus (BUG-20260712-1200's level regression test injects
/// a scripted source). `Send + Sync` so `LogindChannel` keeps the same
/// auto-trait shape it had when it stored a `zbus` proxy directly.
trait InhibitorSource: Send + Sync {
    /// Snapshot the inhibitors held right now. A bus error surfaces as
    /// `Err(ListInhibitorsFailed)`; `poll` degrades that to "no lock
    /// held" so a transient bus hiccup can't pin `call_active` true.
    fn list(&self) -> Result<Vec<InhibitorEntry>, LogindError>;
}

/// Production [`InhibitorSource`] — one `ListInhibitors` round-trip
/// over the `zbus::blocking` system-bus proxy.
struct ZbusInhibitorSource {
    proxy: zbus::blocking::Proxy<'static>,
}

impl InhibitorSource for ZbusInhibitorSource {
    fn list(&self) -> Result<Vec<InhibitorEntry>, LogindError> {
        self.proxy
            .call("ListInhibitors", &())
            .map_err(|_| LogindError::ListInhibitorsFailed)
    }
}

/// Stateful logind inhibitor channel. Holds the loaded whitelist + an
/// [`InhibitorSource`]; `poll()` lists the currently-held inhibitors,
/// classifies each entry, and returns `CallActive` whenever a
/// whitelisted idle-inhibitor is held — the **level**, re-derived from
/// the bus every tick. There is no cross-tick de-dup: a sustained lock
/// reports `CallActive` on every tick it is held, and `None` the tick
/// it is released, so downstream `call_active` tracks the lock's full
/// lifetime (BUG-20260712-1200). The "background subscriber" plumbing
/// (PrepareForSleep + PropertyChanged) is deferred — at 1 Hz daemon
/// cadence re-polling adds no observable latency.
pub struct LogindChannel {
    whitelist: Whitelist,
    source: Box<dyn InhibitorSource>,
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
            source: Box::new(ZbusInhibitorSource { proxy }),
        })
    }
}

impl IntentChannel for LogindChannel {
    /// Level read: `Some(CallActive { who })` while a whitelisted
    /// idle-inhibitor is held, `None` otherwise. A bus error degrades
    /// to `None` (no lock held) rather than latching the previous
    /// value.
    fn poll(&mut self) -> Option<IntentEvent> {
        let entries = self.source.list().ok()?;
        entries
            .iter()
            .find_map(|(what, who, ..)| classify_inhibitor(what, who, &self.whitelist))
    }
}

#[cfg(test)]
impl LogindChannel {
    /// Construct a channel over an injected [`InhibitorSource`] so the
    /// level-semantics tests can script the held-inhibitor list without
    /// a live system bus.
    fn with_source(whitelist: Whitelist, source: Box<dyn InhibitorSource>) -> Self {
        Self { whitelist, source }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Scripted [`InhibitorSource`]: reports a single held `zoom`
    /// idle-inhibitor while `held` is true, an empty list once the flag
    /// flips — models a whitelisted app grabbing then releasing
    /// `Inhibit("idle")`.
    struct HeldSource {
        held: Arc<AtomicBool>,
    }

    impl InhibitorSource for HeldSource {
        fn list(&self) -> Result<Vec<InhibitorEntry>, LogindError> {
            if self.held.load(Ordering::SeqCst) {
                Ok(vec![(
                    "idle".to_string(),
                    "zoom".to_string(),
                    "In a meeting".to_string(),
                    "block".to_string(),
                    1000,
                    4242,
                )])
            } else {
                Ok(Vec::new())
            }
        }
    }

    /// BUG-20260712-1200: `call_active` is a LEVEL, not a one-tick
    /// edge. A held whitelisted idle-inhibitor must make `poll()` return
    /// `CallActive` on EVERY tick it is held (not just the grab edge),
    /// and `None` only once the inhibitor is released. Under the old
    /// `last_emitted` de-dup the second poll returned `None` while the
    /// lock was still held, so telemetry saw `call_active` flip false
    /// after a single tick with no observable release edge.
    #[test]
    fn call_active_is_level_across_ticks() {
        let held = Arc::new(AtomicBool::new(true));
        let mut ch = LogindChannel::with_source(
            canonical_whitelist(),
            Box::new(HeldSource { held: held.clone() }),
        );
        // Held for many ticks → CallActive every single tick.
        for tick in 0..12 {
            assert!(
                matches!(ch.poll(), Some(IntentEvent::CallActive { .. })),
                "tick {tick}: held inhibitor must report CallActive (level)",
            );
        }
        // Release → the very next poll is None (release edge visible).
        held.store(false, Ordering::SeqCst);
        assert_eq!(ch.poll(), None, "released inhibitor must clear call_active");
        // Still None on subsequent ticks — no latched stale value.
        assert_eq!(ch.poll(), None);
    }

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

    /// Regression test for BUG-20260608-2244: `LogindChannel::new` opens
    /// a `zbus::blocking` connection, and the powerd daemon constructs it
    /// from `build_live_intent` while already driving a `current_thread`
    /// tokio runtime. With zbus's `tokio` feature the blocking `block_on`
    /// built its own runtime and aborted the process with "Cannot start a
    /// runtime from within a runtime"; with the `async-io` backend it is
    /// runtime-agnostic and safe. A live system bus is not required —
    /// `Err(BusUnreachable)` (CI / minimal containers) is an acceptable,
    /// non-panicking outcome; only a panic is the regression.
    #[test]
    fn new_does_not_panic_inside_tokio_runtime() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime");
        rt.block_on(async {
            // The result is intentionally discarded: success and
            // BusUnreachable are both fine; a panic is the failure.
            let _ = LogindChannel::new(Path::new("/nonexistent/intent_whitelist.toml"));
        });
    }
}
