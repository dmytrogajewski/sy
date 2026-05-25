//! Popup state for `sy mon`.
//!
//! Roadmap: `specs/roadmaps/sy-mon/ROADMAP.md` Step 16 (initial shape)
//! extended in Step 18 (focus, expand, filter, scroll). Per the spec
//! shape `State { latest, history, banner, focused_panel, expanded,
//! filter, scroll }` — the ring is opened at popup startup so the
//! first `view()` call paints from the historical 600 s window in
//! place of waiting for the first live frame to arrive over
//! `system.mon.subscribe`.
//!
//! The `banner` slot carries the SPEC §6 "Aggregator down → empty
//! popup" risk mitigation. When the IPC subscribe stream fails to
//! connect (or errors mid-flight), the popup sets
//! `banner = Some(Banner { kind: AggregatorDown, last_seen_at_ms })`
//! so the user sees the last cached frame's timestamp alongside the
//! warning. Cleared on the next successful frame.

use regex::Regex;
use sy_core::mon::ring::Ring;
use sy_core::mon::snapshot::SystemSnapshot;

/// Stable identifier for each of the nine SCOPE §4 panels rendered in
/// the popup grid. Variant order is the canonical digit-jump order
/// (`1=Host`, `2=Accel`, `3=Net`, `4=Disk`, `5=Aiplane`, `6=Knowledge`,
/// `7=Agents`, `8=Power`, `9=Supervisor`) — Step 18's `1`..`9` keybind
/// reads this order, and `Tab` cycles forward through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelId {
    Host,
    Accel,
    Net,
    Disk,
    Aiplane,
    Knowledge,
    Agents,
    Power,
    Supervisor,
}

impl PanelId {
    /// Ordered list of all nine panel IDs in digit-jump / Tab-cycle
    /// order. Single source of truth; `from_digit`, `next`, and `prev`
    /// all read this array so a new panel slots in by appending.
    pub const ALL: [PanelId; 9] = [
        PanelId::Host,
        PanelId::Accel,
        PanelId::Net,
        PanelId::Disk,
        PanelId::Aiplane,
        PanelId::Knowledge,
        PanelId::Agents,
        PanelId::Power,
        PanelId::Supervisor,
    ];

    /// Map a 1-based digit (`1`..`9`) to its panel. Returns `None`
    /// for `0` or out-of-range so `keypress_to_message` can fall
    /// through to no-op.
    pub fn from_digit(d: u32) -> Option<PanelId> {
        if (1..=9).contains(&d) {
            Some(PanelId::ALL[(d - 1) as usize])
        } else {
            None
        }
    }

    /// 0-based position of this panel in `ALL`. Used by `next` / `prev`
    /// to cycle without a verbose `match`.
    fn index(self) -> usize {
        PanelId::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }

    /// Cycle forward (Tab). Wraps from `Supervisor` back to `Host`.
    pub fn next(self) -> PanelId {
        PanelId::ALL[(self.index() + 1) % PanelId::ALL.len()]
    }

    /// Cycle backward (Shift+Tab). Wraps from `Host` back to `Supervisor`.
    pub fn prev(self) -> PanelId {
        PanelId::ALL[(self.index() + PanelId::ALL.len() - 1) % PanelId::ALL.len()]
    }
}

/// Banner displayed in the popup chrome when the aggregator is
/// unreachable. Step 16 ships the single [`BannerKind::AggregatorDown`]
/// variant; future risk-mitigation variants (`SchemaDrift`,
/// `RingCorrupt`) slot in alongside without changing the call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    pub kind: BannerKind,
    /// Unix milliseconds of the last successful snapshot frame. The
    /// banner copy reads "data is N seconds old" so the operator can
    /// gauge freshness at a glance.
    pub last_seen_at_ms: u64,
}

/// Reason a banner is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerKind {
    /// `sy mon collect` daemon is unreachable: IPC connect failed or
    /// the streaming subscribe errored mid-flight.
    AggregatorDown,
}

/// Popup state, populated by the IPC subscribe stream and the ring
/// buffer opened at startup.
#[derive(Debug)]
pub struct State {
    /// Most recent snapshot received from `system.mon.subscribe`.
    /// `None` before the first frame lands; populated on every frame.
    pub latest: Option<SystemSnapshot>,
    /// Ring buffer opened at popup startup so the first `view()` call
    /// paints from history instead of waiting for the live stream.
    pub history: Ring,
    /// `Some` when the aggregator is unreachable, carrying the last
    /// successful frame's timestamp for the operator-visible copy.
    pub banner: Option<Banner>,
    /// Currently-focused panel for keyboard navigation (Step 18).
    /// `Tab` cycles forward, `Shift+Tab` backward, `1`..`9` jumps
    /// direct. Defaults to [`PanelId::Host`].
    pub focused_panel: PanelId,
    /// Some when a panel is full-screened (Enter on focused panel).
    /// `view::root` checks this and paints the single panel instead of
    /// the 3×3 grid. Second Enter on the same panel collapses back to
    /// the grid; pressing Enter while a different panel is expanded
    /// also collapses (the test pins toggle behaviour on the
    /// currently-focused panel).
    pub expanded: Option<PanelId>,
    /// Compiled regex from the `/` filter overlay. Per-panel
    /// projections (currently [`crate::mon::view::aiplane`]) drop
    /// metric labels that don't match this pattern. `Esc` clears.
    pub filter: Option<Regex>,
    /// Vertical scroll offset (rows). `j` increments, `k` decrements
    /// — vim convention. View layer clamps against panel height; this
    /// state field is just the raw counter.
    pub scroll: i32,
}

impl State {
    /// Construct a popup state seeded with the on-disk ring. The ring
    /// is opened by the caller (the `sy mon` dispatcher) so this
    /// constructor stays infallible and testable; failure to open the
    /// ring is surfaced earlier in the dispatch path.
    pub fn new(history: Ring) -> Self {
        Self {
            latest: None,
            history,
            banner: None,
            focused_panel: PanelId::Host,
            expanded: None,
            filter: None,
            scroll: 0,
        }
    }
}

/// Pure helper: returns `true` if `name` matches the popup's active
/// filter (or there is no filter). Per-panel projections call this on
/// every metric label they would otherwise emit. An invalid regex
/// (which the user couldn't have produced via the overlay, since the
/// overlay only stores compiled values) returns `true` — fail open.
pub fn metric_matches(filter: &Option<Regex>, name: &str) -> bool {
    match filter {
        Some(re) => re.is_match(name),
        None => true,
    }
}

/// Inputs the [`crate::mon::view::root`] function consumes — exposed
/// as its own struct so `mon::app::tests` can assert on what `view()`
/// will paint without instantiating the iced widget tree. This is the
/// Step 16 "test seam" mitigation: iced 0.14's `Element` is not
/// publicly introspectable, so we factor "what to draw" out of "how
/// to draw it" the same way Step 15's `Recorder` trait did for the
/// canvas widgets.
#[derive(Debug, Clone)]
pub struct ViewData {
    /// History window for column 0 (CPU mean util) — the slice the
    /// scaffolded grid renders for first-paint validation. Oldest
    /// first. Length ≤ ring depth.
    pub cpu_sparkline_recent: Vec<f32>,
    /// Latest snapshot's `captured_at_ms`. `None` before the first
    /// live frame arrives. The scaffolded grid prints this as a
    /// freshness indicator; tests assert on it as a "did the view
    /// see the latest frame" probe.
    pub latest_captured_at_ms: Option<u64>,
    /// Mirror of [`State::banner`] so the view layer can decide
    /// whether to paint the aggregator-down chrome without re-reading
    /// the state.
    pub banner: Option<Banner>,
}

/// Project [`State`] into the inputs the view will paint. Pure
/// function — no I/O, no allocation beyond the returned ring slice
/// — so the Step 16 spec tests can call it and pattern-match on the
/// result.
///
/// The CPU column index is `0`, matching the Step 11 host-sample
/// projection (cpu mean / mem used / swap used / load 1m in cols
/// 0-3). The window matches the SPEC §4 default ring depth (600 s);
/// `Ring::read_metric` caps at what's actually been pushed so an
/// empty ring returns an empty `Vec`.
pub fn view_data(state: &State) -> ViewData {
    // CPU mean util lives in ring column 0 per the Step 11 sample-
    // projection contract. A failed read here means the ring shape
    // is out of sync with the column map — surface as an empty
    // sparkline; the operator already has the aggregator-down banner
    // for the user-visible signal.
    let cpu_sparkline_recent = state
        .history
        .read_metric(0, super::cli::DEFAULT_HISTORY_SIZE as usize)
        .unwrap_or_default();
    ViewData {
        cpu_sparkline_recent,
        latest_captured_at_ms: state.latest.as_ref().map(|s| s.captured_at_ms),
        banner: state.banner.clone(),
    }
}
