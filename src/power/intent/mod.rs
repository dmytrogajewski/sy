//! Intent channels — the application-layer half of the 12-signal
//! panel (SPEC §2 "Deep Dives — ML choice rationale"). Each channel
//! is a stateful, non-blocking source of `IntentEvent`s the daemon
//! drains every 1 Hz tick (and after epoll wakes for the ones that
//! plumb a pollable fd).
//!
//! Step 4 lands `psi` — the PSI cgroup-v2 trigger watcher whose
//! sub-second leading-edge detection on builds is the SPEC's single
//! biggest unique signal vs PMU-counter-only baselines. Steps 5-7
//! add logind, niri toplevel, aiplane registry, MPRIS, idle, etc.
//!
//! The trait deliberately takes `&mut self` (unlike `Sensor::read`,
//! which is stateless and takes `&self`) because channels hold
//! watcher state — open file descriptors, last-event timestamps,
//! cached cgroup paths.

pub mod aiplane;
pub mod cgroup;
pub mod idle;
pub mod logind;
pub mod mpris;
pub mod niri;
pub mod notify;
pub mod portal;
pub mod psi;
pub mod time;

pub use aiplane::AiplaneIntentChannel;
pub use cgroup::CgroupAncestryChannel;
pub use idle::IdleChannel;
pub use logind::LogindChannel;
pub use mpris::MprisChannel;
pub use niri::NiriChannel;
pub use notify::NotifyChannel;
pub use portal::ScreenCastChannel;
pub use psi::{PsiChannel, PsiKind};
pub use time::TimeChannel;

/// One application-layer event harvested from an intent channel.
/// Step 4 ships the PSI variant; Step 5 adds `CallActive` (logind
/// inhibitor watcher); Step 6: `FocusedAppChanged`, `NpuQueue`;
/// Step 7: `MediaPlaying`, `ScreenCastActive`, `UserIdle`,
/// `ProcessFromAncestor`, `FanComplaint`, `TimeOfDay`.
#[derive(Debug, Clone, PartialEq)]
pub enum IntentEvent {
    /// PSI pressure crossed the trigger threshold. `since_ms` is the
    /// monotonic time in ms since the channel was constructed —
    /// downstream uses this as the leading-edge timestamp.
    PsiSpike { kind: PsiKind, since_ms: u64 },
    /// A whitelisted app (Teams / Slack / Zoom / Discord) holds an
    /// `Inhibit("idle")` lock on systemd-logind — the de-facto Linux
    /// "user is in a call" signal (SPEC §2). `who` carries the raw
    /// inhibitor `Who` field so the daemon can de-dup and emit a
    /// stable provenance string in the snapshot.
    CallActive { who: String },
    /// The niri compositor's focused window changed. **Only `app_id`
    /// crosses the boundary** — per SPEC §4 "Privacy", raw window
    /// titles are stripped at the niri-event parser, never carried
    /// into any snapshot or downstream feature.
    FocusedAppChanged { app_id: String },
    /// In-process tap of `aiplane::Registry::current_queue_depth()`:
    /// `depth` is the number of workloads currently in
    /// `Registry::run`, `head_workload` is the kind name of the most
    /// recently dispatched workload (None when the registry has never
    /// run anything). Zero IPC — same-process Arc read.
    NpuQueue {
        depth: usize,
        head_workload: Option<String>,
    },
    /// At least one MPRIS player on the session bus reports
    /// `PlaybackStatus == "Playing"` — coarse "media is playing"
    /// signal. No player identity / track title crosses the boundary.
    MediaPlaying,
    /// A non-zero count of `org.freedesktop.portal.ScreenCast`
    /// sessions is currently active — proxy for screen-share / call.
    ScreenCastActive,
    /// Wayland `ext-idle-notify-v1` reported the user idle for
    /// `since_ms` milliseconds. Stubbed to `0` until a Wayland-client
    /// dep lands (SPEC §4 "Step 7 known limitation" — see
    /// `intent::idle`).
    UserIdle { since_ms: u64 },
    /// A new process appeared under a cgroup ancestor that matches
    /// the configured allow-list (e.g. `firefox.scope` ⇒ `"firefox"`).
    /// Only the matched ancestor token crosses the boundary.
    ProcessFromAncestor { name: String },
    /// A notification body contained a coarse "fan complaint" keyword
    /// (`fan` / `loud` / `noisy`). **The body text itself is
    /// discarded at the boundary** per SPEC §4 Privacy.
    FanComplaint,
    /// Cyclical encoding of the current time-of-day + day-of-week.
    /// `sin`/`cos` pair maps hour∈[0,24) onto the unit circle so the
    /// learner sees 23:59 ≈ 00:01; `dow_sin`/`dow_cos` does the same
    /// for day-of-week (Sunday→Monday continuity).
    TimeOfDay {
        sin: f32,
        cos: f32,
        dow_sin: f32,
        dow_cos: f32,
    },
}

/// Non-blocking poll over an intent source. Returns `None` when
/// nothing is pending. The daemon (Step 10) calls `poll()` once per
/// 1 Hz tick across every registered channel; channels backed by a
/// pollable fd may additionally wake the daemon via epoll between
/// ticks (PSI in particular fires within a 150 ms window — see
/// `psi::PsiChannel`).
pub trait IntentChannel {
    fn poll(&mut self) -> Option<IntentEvent>;
}
