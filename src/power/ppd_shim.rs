//! `net.hadess.PowerProfiles` D-Bus shim — Step 36 of `sy-power`
//! roadmap (Phase R7).
//!
//! GNOME's quick-settings power tile speaks the `net.hadess.PowerProfiles`
//! protocol (the same protocol `power-profiles-daemon` implements). This
//! shim re-exposes the same wire surface but routes profile changes
//! through `sy power`'s pin slot — so flipping GNOME's tile to
//! `performance` translates into pinning the `build` bandit arm.
//!
//! ## Profile mapping (SPEC §3 "PPD replacement")
//!
//! | PPD profile name | sy bandit arm |
//! |------------------|---------------|
//! | `power-saver`    | `idle`        |
//! | `balanced`       | `code`        |
//! | `performance`    | `build`       |
//!
//! The three PPD names are the canonical `power-profiles-daemon`
//! profiles; the three arm names are `sy power`'s pinable analogues
//! per SPEC §4 ("Bandit Arms"). The mapping is fixed and total over
//! the PPD-side domain — `SetActiveProfile("anything-else")` is
//! rejected with an `org.freedesktop.DBus.Error.InvalidArgs`.
//!
//! ## What the shim owns
//!
//! - `active` — the PPD-side label the shim last accepted, mirroring
//!   what GNOME's quick-settings shows.
//! - `holds` — a cookie-keyed registry of `HoldProfile` calls. The PPD
//!   protocol allows stacked holds; the most recent hold's profile
//!   becomes the effective one. When the registry empties, the
//!   user's last `SetActiveProfile` choice resumes.
//! - A handle to the daemon's [`LatestPin`] slot. Every effective-arm
//!   change writes through to the pin so the next tick of `one_tick`
//!   picks the operator-chosen arm.
//!
//! ## Step boundary
//!
//! Step 36 ships the shim itself + the `#[zbus::interface]` glue.
//! Step 37 adds the `--with-ppd` opt-out + the system-bus name-bind
//! conditional. Step 38 adds the MCP `power_status` tool. The
//! integration test that actually opens a bus is gated by
//! `cfg(feature = "test-dbus")` so the default `make test` run
//! stays bus-free.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use crate::power::bandit::Arm;
use crate::power::daemon::LatestPin;

/// Canonical PPD profile label for the low-power tier.
pub const PPD_POWER_SAVER: &str = "power-saver";
/// Canonical PPD profile label for the default tier.
pub const PPD_BALANCED: &str = "balanced";
/// Canonical PPD profile label for the high-performance tier.
pub const PPD_PERFORMANCE: &str = "performance";

/// `sy power`'s arm name pinned when PPD reports `power-saver`.
pub const ARM_FOR_POWER_SAVER: &str = "idle";
/// `sy power`'s arm name pinned when PPD reports `balanced`.
pub const ARM_FOR_BALANCED: &str = "code";
/// `sy power`'s arm name pinned when PPD reports `performance`.
pub const ARM_FOR_PERFORMANCE: &str = "build";

/// Default PPD-side profile the shim reports until the operator (or a
/// hold) flips it. Matches PPD's own default.
pub const DEFAULT_ACTIVE_PROFILE: &str = PPD_BALANCED;

/// Map a PPD profile label to its `sy power` arm name, or `None` if
/// the label is outside the documented three-profile domain.
///
/// `SetActiveProfile` + `HoldProfile` both validate against this map
/// before accepting an argument; the GNOME quick-settings tile only
/// ever sends these three values, but other clients (e.g. `gdbus
/// call`) can send arbitrary strings.
pub fn arm_for_profile(profile: &str) -> Option<&'static str> {
    match profile {
        PPD_POWER_SAVER => Some(ARM_FOR_POWER_SAVER),
        PPD_BALANCED => Some(ARM_FOR_BALANCED),
        PPD_PERFORMANCE => Some(ARM_FOR_PERFORMANCE),
        _ => None,
    }
}

/// Outbox for `ActiveProfile` property-changed signals. The production
/// path wires this to zbus's `PropertiesChanged` emitter; tests wire
/// a vector-recording mock so they can assert exact-one-emit on every
/// state transition without spinning up a bus.
pub trait SignalEmitter: Send + Sync {
    /// Called every time the effective PPD profile changes (operator
    /// `SetActiveProfile`, new hold takes effect, last hold released).
    /// `new` is one of the three canonical PPD labels.
    fn emit_active_profile_changed(&self, new: &str);
}

/// `SignalEmitter` impl that drops every event. Used by the daemon
/// before the zbus connection is established + by tests that don't
/// care about the signal path.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullEmitter;

impl SignalEmitter for NullEmitter {
    fn emit_active_profile_changed(&self, _new: &str) {}
}

/// One entry in the [`PpdShim::holds`] registry. Mirrors PPD's own
/// `ActiveProfileHolds` payload (the `aa{sv}` property): profile name,
/// human-readable reason, and the `application_id` that requested the
/// hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldEntry {
    /// PPD profile label being held — already validated against the
    /// canonical three-profile domain.
    pub profile: String,
    /// Free-form reason supplied by the caller.
    pub reason: String,
    /// Reverse-DNS application identifier supplied by the caller.
    pub application_id: String,
}

/// The PPD-protocol shim. Owns the active-profile label, the
/// `HoldProfile` cookie registry, and a handle to the daemon's pin
/// slot. Every state-mutating method also pushes the new effective
/// profile through `emitter` so the zbus `PropertiesChanged` signal
/// fires.
///
/// `PpdShim` is `Clone` so the zbus object-server can take an owned
/// copy while the daemon keeps another for cookie-management helpers.
#[derive(Clone)]
pub struct PpdShim {
    pin: LatestPin,
    active: Arc<RwLock<String>>,
    holds: Arc<RwLock<HashMap<u32, HoldEntry>>>,
    hold_order: Arc<RwLock<Vec<u32>>>,
    next_cookie: Arc<AtomicU32>,
    arms: Arc<Vec<Arm>>,
    emitter: Arc<dyn SignalEmitter>,
}

impl PpdShim {
    /// Build a shim wired to `pin` (the daemon's [`LatestPin`] slot),
    /// using `arms` as the validation table (the arm a PPD profile
    /// maps to must actually exist in the loaded `power.toml`), and
    /// `emitter` as the `PropertiesChanged` outbox.
    pub fn new(pin: LatestPin, arms: Vec<Arm>, emitter: Arc<dyn SignalEmitter>) -> Self {
        Self {
            pin,
            active: Arc::new(RwLock::new(DEFAULT_ACTIVE_PROFILE.to_string())),
            holds: Arc::new(RwLock::new(HashMap::new())),
            hold_order: Arc::new(RwLock::new(Vec::new())),
            next_cookie: Arc::new(AtomicU32::new(1)),
            arms: Arc::new(arms),
            emitter,
        }
    }

    /// Return the currently effective PPD profile label. When at
    /// least one hold is active, this is the most-recently-issued
    /// hold's profile; otherwise it's the operator's last
    /// `SetActiveProfile` choice (or [`DEFAULT_ACTIVE_PROFILE`]).
    pub fn effective_profile(&self) -> String {
        self.compute_effective()
    }

    /// Accept a new operator-set profile. Validates against the
    /// canonical three-profile domain + the loaded arm table; on
    /// accept, writes the mapped arm into the pin slot and emits a
    /// `PropertiesChanged` signal when the effective profile shifts.
    ///
    /// Returns `Err(InvalidArgs)` for an out-of-domain label, mirroring
    /// PPD's own behaviour so GNOME-side error paths exercise the same
    /// branch.
    pub fn apply_active_profile(&self, value: &str) -> Result<(), PpdShimError> {
        let arm = self.validate_and_map(value)?;
        let prev_effective = self.compute_effective();
        if let Ok(mut g) = self.active.write() {
            *g = value.to_string();
        }
        // Pin only flips when no hold is active — holds win over the
        // operator's baseline choice per the PPD protocol.
        if self.holds.read().map(|h| h.is_empty()).unwrap_or(true) {
            self.write_pin(arm);
        }
        let next_effective = self.compute_effective();
        if next_effective != prev_effective {
            self.emitter.emit_active_profile_changed(&next_effective);
        }
        Ok(())
    }

    /// Register a hold. Returns the cookie the caller will pass back
    /// to [`Self::release_hold`]. Validates the profile against the
    /// canonical domain.
    pub fn register_hold(
        &self,
        profile: &str,
        reason: &str,
        application_id: &str,
    ) -> Result<u32, PpdShimError> {
        let arm = self.validate_and_map(profile)?;
        let prev_effective = self.compute_effective();
        let cookie = self.next_cookie.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut h) = self.holds.write() {
            h.insert(
                cookie,
                HoldEntry {
                    profile: profile.to_string(),
                    reason: reason.to_string(),
                    application_id: application_id.to_string(),
                },
            );
        }
        if let Ok(mut order) = self.hold_order.write() {
            order.push(cookie);
        }
        // New hold becomes the effective choice — pin flips to its arm.
        self.write_pin(arm);
        let next_effective = self.compute_effective();
        if next_effective != prev_effective {
            self.emitter.emit_active_profile_changed(&next_effective);
        }
        Ok(cookie)
    }

    /// Drop the hold with `cookie`. If `cookie` isn't registered
    /// returns `Err(InvalidArgs)` (matching PPD). When the registry
    /// empties, the pin reverts to the operator's last
    /// `apply_active_profile`.
    pub fn release_hold(&self, cookie: u32) -> Result<(), PpdShimError> {
        let prev_effective = self.compute_effective();
        let existed = self
            .holds
            .write()
            .ok()
            .and_then(|mut h| h.remove(&cookie))
            .is_some();
        if !existed {
            return Err(PpdShimError::UnknownCookie);
        }
        if let Ok(mut order) = self.hold_order.write() {
            order.retain(|c| *c != cookie);
        }
        // Recompute effective. If holds remain, pin to the new
        // most-recent hold; otherwise pin to the operator's baseline.
        let next_effective = self.compute_effective();
        if let Some(arm) = arm_for_profile(&next_effective) {
            self.write_pin(arm);
        }
        if next_effective != prev_effective {
            self.emitter.emit_active_profile_changed(&next_effective);
        }
        Ok(())
    }

    /// Snapshot of every live hold, in insertion order. Used to build
    /// the `ActiveProfileHolds` `aa{sv}` property payload.
    pub fn active_holds(&self) -> Vec<HoldEntry> {
        let order = self
            .hold_order
            .read()
            .map(|o| o.clone())
            .unwrap_or_default();
        let holds = self.holds.read();
        let Ok(holds) = holds else {
            return Vec::new();
        };
        order.iter().filter_map(|c| holds.get(c).cloned()).collect()
    }

    /// Validate a profile label and resolve the bandit arm name.
    /// Rejects labels outside the canonical three-profile domain + any
    /// label whose mapped arm isn't in the loaded `power.toml`.
    fn validate_and_map(&self, profile: &str) -> Result<&'static str, PpdShimError> {
        let arm = arm_for_profile(profile).ok_or(PpdShimError::InvalidProfile)?;
        if self.arms.iter().any(|a| a.name == arm) {
            Ok(arm)
        } else {
            Err(PpdShimError::ArmNotLoaded)
        }
    }

    /// Compute the currently effective PPD profile: most-recent hold
    /// wins; falls back to the operator's `active`.
    fn compute_effective(&self) -> String {
        if let Ok(order) = self.hold_order.read() {
            if let Some(latest) = order.last() {
                if let Ok(holds) = self.holds.read() {
                    if let Some(entry) = holds.get(latest) {
                        return entry.profile.clone();
                    }
                }
            }
        }
        self.active
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| DEFAULT_ACTIVE_PROFILE.to_string())
    }

    /// Write `arm` into the daemon's pin slot. Errors are swallowed —
    /// a poisoned lock means another thread panicked, which the daemon
    /// surface already handles via the watchdog path.
    fn write_pin(&self, arm: &str) {
        if let Ok(mut g) = self.pin.write() {
            *g = Some(arm.to_string());
        }
    }
}

/// Error variants emitted by the pure-Rust shim methods. Mapped to
/// `org.freedesktop.DBus.Error.InvalidArgs` at the zbus boundary so
/// GNOME-side error paths see the canonical PPD error class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PpdShimError {
    /// Profile label is not one of `power-saver`/`balanced`/`performance`.
    InvalidProfile,
    /// The arm the profile maps to is not present in the loaded
    /// `power.toml`. Indicates a misconfigured deployment — the
    /// operator's `arms = [...]` list is missing one of `idle`,
    /// `code`, or `build`.
    ArmNotLoaded,
    /// `ReleaseProfile` called with a cookie that isn't registered.
    UnknownCookie,
}

impl std::fmt::Display for PpdShimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfile => {
                f.write_str("profile must be one of: power-saver, balanced, performance")
            }
            Self::ArmNotLoaded => f.write_str(
                "mapped sy-power arm is not in the loaded power.toml — \
                 deployment misconfigured",
            ),
            Self::UnknownCookie => f.write_str("no hold registered for the given cookie"),
        }
    }
}

impl std::error::Error for PpdShimError {}

impl From<PpdShimError> for zbus::fdo::Error {
    fn from(value: PpdShimError) -> Self {
        zbus::fdo::Error::InvalidArgs(value.to_string())
    }
}

/// zbus interface impl — the wire surface. Every method delegates to
/// the pure-Rust `PpdShim` methods above; the macro emits the
/// `PropertiesChanged` signal for `#[zbus(property)]` writes via the
/// `ActiveProfile` setter.
#[zbus::interface(name = "net.hadess.PowerProfiles")]
impl PpdShim {
    /// Read the currently effective PPD profile label.
    #[zbus(property)]
    fn active_profile(&self) -> String {
        self.effective_profile()
    }

    /// Operator-set profile change. Returns `InvalidArgs` for an
    /// out-of-domain label.
    #[zbus(property)]
    fn set_active_profile(&self, value: String) -> zbus::fdo::Result<()> {
        self.apply_active_profile(&value).map_err(Into::into)
    }

    /// PPD's `PerformanceDegraded` property. `sy power` never reports
    /// a degraded state through this surface (the SPEC §5 status JSON
    /// is the canonical degradation channel), so this always returns
    /// the empty string per PPD's "no degradation" convention.
    #[zbus(property)]
    fn performance_degraded(&self) -> String {
        String::new()
    }

    /// PPD's `Profiles` `aa{sv}` property — the list of profiles the
    /// shim accepts. Step 36 returns an empty list because GNOME's
    /// quick-settings only consults `ActiveProfile`; Step 37 extends
    /// this with the full profile metadata.
    #[zbus(property)]
    fn profiles(&self) -> Vec<std::collections::HashMap<String, zbus::zvariant::OwnedValue>> {
        Vec::new()
    }

    /// PPD's `Actions` `as` property — list of action names the shim
    /// supports. `sy power`'s actuator menu is internal; nothing is
    /// exposed through this PPD field.
    #[zbus(property)]
    fn actions(&self) -> Vec<String> {
        Vec::new()
    }

    /// PPD's `ActiveProfileHolds` `aa{sv}` property. Each entry has
    /// `Profile`, `Reason`, `ApplicationId` keys.
    #[zbus(property)]
    fn active_profile_holds(
        &self,
    ) -> Vec<std::collections::HashMap<String, zbus::zvariant::OwnedValue>> {
        self.active_holds()
            .into_iter()
            .map(hold_entry_to_dict)
            .collect()
    }

    /// PPD `HoldProfile` method. Registers a hold and returns the
    /// cookie the caller must later pass to `ReleaseProfile`.
    fn hold_profile(
        &self,
        profile: &str,
        reason: &str,
        application_id: &str,
    ) -> zbus::fdo::Result<u32> {
        self.register_hold(profile, reason, application_id)
            .map_err(Into::into)
    }

    /// PPD `ReleaseProfile` method. Drops the hold associated with
    /// `cookie`; the effective profile recomputes from the remaining
    /// holds + the operator baseline.
    fn release_profile(&self, cookie: u32) -> zbus::fdo::Result<()> {
        self.release_hold(cookie).map_err(Into::into)
    }
}

/// D-Bus object path the GNOME quick-settings tile listens on.
/// PPD's canonical path; any deviation makes us invisible to GNOME.
pub const PPD_OBJECT_PATH: &str = "/net/hadess/PowerProfiles";

/// D-Bus well-known name. Step 36 always bound it; Step 37 introduces
/// the `bind_name: bool` toggle on [`spawn_system_bus_shim`] so the
/// `--with-ppd` opt-out keeps `power-profiles-daemon` as the
/// authoritative `net.hadess.PowerProfiles` owner.
pub const PPD_WELL_KNOWN_NAME: &str = "net.hadess.PowerProfiles";

/// Probe interface for "who currently owns this D-Bus well-known
/// name?" — the indirection the Step P3-3 auto-detection hangs off
/// of so unit tests can exercise both branches (`tuned-ppd` owns the
/// name vs. nobody owns it) without spinning up a real system bus.
///
/// Production uses [`SystemBusProbe`]; tests inject a canned mock.
pub trait NameOwnerProbe {
    /// Return the unique bus name (e.g. `:1.35`) of the current owner
    /// of `well_known`, or `None` if the name is unbound. Any
    /// transient zbus error must map to `None` so the caller treats
    /// the probe as "no owner detected" and falls forward to its
    /// existing claim-attempt path (which will surface the real
    /// failure at WARN level if the bus is actually unreachable).
    fn current_owner(&self, well_known: &str) -> Option<String>;
}

/// Real `NameOwnerProbe` impl: opens a transient blocking system-bus
/// connection and calls `org.freedesktop.DBus.GetNameOwner`. The
/// connection is dropped at the end of the call — the long-lived
/// shim connection (when the claim succeeds) is built separately by
/// [`run_blocking_shim`].
pub struct SystemBusProbe;

impl NameOwnerProbe for SystemBusProbe {
    fn current_owner(&self, well_known: &str) -> Option<String> {
        let conn = zbus::blocking::Connection::system().ok()?;
        let proxy = zbus::blocking::fdo::DBusProxy::new(&conn).ok()?;
        let name = zbus::names::BusName::try_from(well_known).ok()?;
        proxy.get_name_owner(name).ok().map(|n| n.to_string())
    }
}

/// Outcome of the bind-decision logic: either claim the name or park
/// in observer mode. `Skip` carries the unique-name of the existing
/// owner (or the literal `--with-ppd` opt-out marker) so the
/// startup INFO log explains WHY the shim isn't bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindDecision {
    /// Proceed to claim [`PPD_WELL_KNOWN_NAME`] on the system bus.
    Claim,
    /// Skip the claim — either the operator opted out (`bind_name =
    /// false`) or another peer (`tuned-ppd.service`,
    /// `power-profiles-daemon`, etc.) already owns the name.
    Skip {
        /// Identifier for the INFO log line. Either a unique bus
        /// name (`:1.35`) or the literal opt-out marker.
        owner: String,
    },
}

/// Marker used in the `Skip { owner }` payload when the operator set
/// `SY_POWER_WITH_PPD=1` / passed `--with-ppd`. Distinct from any
/// real unique-name format so log readers can grep for it.
pub const WITH_PPD_OPT_OUT_MARKER: &str = "--with-ppd opt-out";

/// Resolve whether the shim should claim the well-known PPD bus
/// name. Order of precedence:
///
/// 1. `bind_name = false` (operator opt-out) → `Skip` immediately;
///    the probe is NOT consulted.
/// 2. `probe.current_owner(PPD_WELL_KNOWN_NAME) == Some(owner)` →
///    `Skip { owner }` (observer mode); avoids the
///    "name already taken on the bus" WARN spam on hosts where
///    `tuned-ppd.service` is active.
/// 3. Otherwise → `Claim`.
pub fn decide_bind(bind_name: bool, probe: &dyn NameOwnerProbe) -> BindDecision {
    if !bind_name {
        return BindDecision::Skip {
            owner: WITH_PPD_OPT_OUT_MARKER.to_string(),
        };
    }
    match probe.current_owner(PPD_WELL_KNOWN_NAME) {
        Some(owner) => BindDecision::Skip { owner },
        None => BindDecision::Claim,
    }
}

/// Emit the one-shot startup INFO line that explains WHY the shim is
/// running in observer mode. No-op for [`BindDecision::Claim`] —
/// `run_blocking_shim` logs its own "bound" line on the happy path.
pub fn log_bind_decision(decision: &BindDecision) {
    if let BindDecision::Skip { owner } = decision {
        tracing::info!(
            target: "sy::power::ppd_shim",
            owner = %owner,
            "PPD name owned by {owner}; running shim in observer mode",
        );
    }
}

/// Spawn the PPD D-Bus shim onto a blocking thread.
///
/// - `bind_name = true` (default `sy power apply --yes` flow): probe
///   the system bus for [`PPD_WELL_KNOWN_NAME`] ownership via
///   [`SystemBusProbe`]. If unowned, claim the name; if a peer (e.g.
///   `tuned-ppd.service`) already owns it, degrade silently to
///   observer mode (one INFO line at startup, no per-restart WARN).
/// - `bind_name = false` (`--with-ppd` co-existence flow): skip the
///   bus-name claim entirely — the shim thread parks idle so PPD
///   keeps the name and there is no bus-name fight at startup. The
///   daemon's actuation loop still applies the pin slot from whatever
///   path mutates it, so this only disables the GNOME-tile bridge.
///
/// Returns immediately — the returned `JoinHandle` is the worker
/// thread the shim lives in; dropping it does NOT tear the connection
/// down (zbus's blocking `Connection` holds its own background
/// thread).
///
/// On any bus-bind error (no `DBUS_SYSTEM_BUS_ADDRESS`,
/// polkit/AppArmor refusal), the helper logs at `warn` level and the
/// daemon's actuation loop continues uninterrupted — the PPD surface
/// is a UX add-on, not a critical path.
pub fn spawn_system_bus_shim(
    pin: LatestPin,
    arms: Vec<Arm>,
    bind_name: bool,
) -> std::thread::JoinHandle<()> {
    let decision = decide_bind(bind_name, &SystemBusProbe);
    spawn_system_bus_shim_with_decision(pin, arms, decision)
}

/// Inner spawn helper — branches on a pre-resolved [`BindDecision`]
/// so the probe call site stays out of the spawned thread (and out
/// of tests that don't want a real bus connection).
fn spawn_system_bus_shim_with_decision(
    pin: LatestPin,
    arms: Vec<Arm>,
    decision: BindDecision,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("sy-power-ppd-shim".to_string())
        .spawn(move || {
            log_bind_decision(&decision);
            if matches!(decision, BindDecision::Skip { .. }) {
                loop {
                    std::thread::park();
                }
            }
            if let Err(e) = run_blocking_shim(pin, arms) {
                tracing::warn!(
                    target: "sy::power::ppd_shim",
                    error = %e,
                    "PPD D-Bus shim bind failed; GNOME quick-settings will not see sy power"
                );
            }
        })
        .expect("spawn ppd-shim thread")
}

/// Build the blocking zbus connection, bind the well-known name, and
/// serve the shim at [`PPD_OBJECT_PATH`]. The returned `Connection`
/// is intentionally held in a `_conn` binding for the duration of the
/// `park_forever` loop — dropping it would close the bus connection
/// and remove the well-known name.
fn run_blocking_shim(pin: LatestPin, arms: Vec<Arm>) -> Result<(), zbus::Error> {
    let shim = PpdShim::new(pin, arms, Arc::new(NullEmitter));
    let _conn = zbus::blocking::connection::Builder::system()?
        .name(PPD_WELL_KNOWN_NAME)?
        .serve_at(PPD_OBJECT_PATH, shim)?
        .build()?;
    tracing::info!(
        target: "sy::power::ppd_shim",
        bus_name = PPD_WELL_KNOWN_NAME,
        object_path = PPD_OBJECT_PATH,
        "PPD shim bound; GNOME quick-settings tile is now wired to sy power"
    );
    // Park indefinitely — the daemon's signal handler exits the
    // process on SIGTERM, which drops this thread + `_conn` together.
    loop {
        std::thread::park();
    }
}

/// Build a single-entry dict mirroring PPD's `a{sv}` shape. zvariant's
/// `OwnedValue::from` on a `&str` produces a string variant; PPD only
/// uses string-typed variants for the hold record so this stays
/// type-uniform.
fn hold_entry_to_dict(
    entry: HoldEntry,
) -> std::collections::HashMap<String, zbus::zvariant::OwnedValue> {
    use zbus::zvariant::Value;
    let mut out = std::collections::HashMap::with_capacity(3);
    if let Ok(v) = Value::from(entry.profile.as_str()).try_to_owned() {
        out.insert("Profile".to_string(), v);
    }
    if let Ok(v) = Value::from(entry.reason.as_str()).try_to_owned() {
        out.insert("Reason".to_string(), v);
    }
    if let Ok(v) = Value::from(entry.application_id.as_str()).try_to_owned() {
        out.insert("ApplicationId".to_string(), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::bandit::{Arm, CgroupOverrides, Epp, NpuPmode};
    use crate::power::daemon::new_pin_slot;
    use crate::power::sensors::igpu::IgpuProfileMode;
    use crate::power::sensors::platform::PlatformProfile;
    use std::sync::Mutex;

    /// Build a minimal arm table covering the three PPD-mapped arm
    /// names; mirrors what `configs/sy/power.toml` ships but without
    /// the eight-arm fanout so the test stays focused.
    fn three_arm_table() -> Vec<Arm> {
        [ARM_FOR_POWER_SAVER, ARM_FOR_BALANCED, ARM_FOR_PERFORMANCE]
            .iter()
            .map(|name| Arm {
                name: (*name).to_string(),
                platform_profile: PlatformProfile::Balanced,
                epp: Epp::BalancePerformance,
                igpu_mode: IgpuProfileMode::BootupDefault,
                npu_pmode: NpuPmode::Default,
                cgroup: CgroupOverrides::default(),
            })
            .collect()
    }

    /// Recording `SignalEmitter` — pushes every fired event into a
    /// shared `Vec` so tests can assert exact-one-emit.
    #[derive(Debug, Default, Clone)]
    struct RecordingEmitter {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl SignalEmitter for RecordingEmitter {
        fn emit_active_profile_changed(&self, new: &str) {
            if let Ok(mut g) = self.events.lock() {
                g.push(new.to_string());
            }
        }
    }

    impl RecordingEmitter {
        fn snapshot(&self) -> Vec<String> {
            self.events.lock().map(|g| g.clone()).unwrap_or_default()
        }
    }

    /// Step 36 named DoD test: `SetActiveProfile("performance")` flips
    /// the daemon's pin slot to `build`, and a subsequent
    /// `GetActiveProfile` returns `"performance"`.
    #[test]
    fn active_profile_round_trip() {
        let pin = new_pin_slot();
        let shim = PpdShim::new(Arc::clone(&pin), three_arm_table(), Arc::new(NullEmitter));
        shim.apply_active_profile(PPD_PERFORMANCE)
            .expect("set performance");
        let pinned = pin.read().expect("pin lock").clone();
        assert_eq!(pinned.as_deref(), Some(ARM_FOR_PERFORMANCE));
        assert_eq!(shim.effective_profile(), PPD_PERFORMANCE);
    }

    /// Step 36 named DoD test: every effective-profile transition
    /// fires exactly one `PropertiesChanged` event; a no-op
    /// `SetActiveProfile` (same value) fires zero.
    #[test]
    fn change_signal_emitted_on_arm_flip() {
        let pin = new_pin_slot();
        let emitter = RecordingEmitter::default();
        let shim = PpdShim::new(
            Arc::clone(&pin),
            three_arm_table(),
            Arc::new(emitter.clone()),
        );
        // Default is `balanced`; flipping to `performance` is one event.
        shim.apply_active_profile(PPD_PERFORMANCE).expect("flip 1");
        // Re-applying the same value emits zero — PPD's contract.
        shim.apply_active_profile(PPD_PERFORMANCE).expect("flip 2");
        // Flipping to `power-saver` emits one more.
        shim.apply_active_profile(PPD_POWER_SAVER).expect("flip 3");
        assert_eq!(
            emitter.snapshot(),
            vec![PPD_PERFORMANCE.to_string(), PPD_POWER_SAVER.to_string()],
        );
    }

    /// Out-of-domain profile labels are rejected; the pin slot stays
    /// untouched.
    #[test]
    fn rejects_unknown_profile() {
        let pin = new_pin_slot();
        let shim = PpdShim::new(Arc::clone(&pin), three_arm_table(), Arc::new(NullEmitter));
        let err = shim.apply_active_profile("turbo").unwrap_err();
        assert_eq!(err, PpdShimError::InvalidProfile);
        assert!(pin.read().expect("pin lock").is_none());
    }

    /// `HoldProfile` overrides the operator's baseline; releasing the
    /// hold reverts to the baseline arm.
    #[test]
    fn hold_overrides_baseline_until_released() {
        let pin = new_pin_slot();
        let shim = PpdShim::new(Arc::clone(&pin), three_arm_table(), Arc::new(NullEmitter));
        shim.apply_active_profile(PPD_BALANCED).expect("baseline");
        let cookie = shim
            .register_hold(PPD_PERFORMANCE, "compile burst", "sy.test")
            .expect("hold");
        assert_eq!(
            pin.read().expect("pin lock").as_deref(),
            Some(ARM_FOR_PERFORMANCE),
        );
        assert_eq!(shim.effective_profile(), PPD_PERFORMANCE);
        shim.release_hold(cookie).expect("release");
        assert_eq!(
            pin.read().expect("pin lock").as_deref(),
            Some(ARM_FOR_BALANCED),
        );
        assert_eq!(shim.effective_profile(), PPD_BALANCED);
    }

    /// `ReleaseProfile` with an unknown cookie returns an error and
    /// leaves the registry untouched.
    #[test]
    fn release_unknown_cookie_errors() {
        let pin = new_pin_slot();
        let shim = PpdShim::new(pin, three_arm_table(), Arc::new(NullEmitter));
        let err = shim.release_hold(9999).unwrap_err();
        assert_eq!(err, PpdShimError::UnknownCookie);
    }

    /// The most-recent hold wins. Releasing the most-recent hold
    /// reveals the next-most-recent one.
    #[test]
    fn stacked_holds_lifo_order() {
        let pin = new_pin_slot();
        let shim = PpdShim::new(Arc::clone(&pin), three_arm_table(), Arc::new(NullEmitter));
        let saver = shim
            .register_hold(PPD_POWER_SAVER, "thermal", "sy.test")
            .expect("hold saver");
        let perf = shim
            .register_hold(PPD_PERFORMANCE, "burst", "sy.test")
            .expect("hold perf");
        // Most-recent hold (`perf`) is effective.
        assert_eq!(shim.effective_profile(), PPD_PERFORMANCE);
        shim.release_hold(perf).expect("release perf");
        // Falls back to the older hold.
        assert_eq!(shim.effective_profile(), PPD_POWER_SAVER);
        shim.release_hold(saver).expect("release saver");
        // No holds left — operator baseline (`balanced` default) wins.
        assert_eq!(shim.effective_profile(), PPD_BALANCED);
    }

    /// `arm_for_profile` covers every canonical profile and rejects
    /// the obvious typos.
    #[test]
    fn arm_for_profile_total_over_canonical_domain() {
        assert_eq!(arm_for_profile(PPD_POWER_SAVER), Some(ARM_FOR_POWER_SAVER));
        assert_eq!(arm_for_profile(PPD_BALANCED), Some(ARM_FOR_BALANCED));
        assert_eq!(arm_for_profile(PPD_PERFORMANCE), Some(ARM_FOR_PERFORMANCE));
        assert_eq!(arm_for_profile("powersaver"), None);
        assert_eq!(arm_for_profile(""), None);
    }

    /// `NameOwnerProbe` impl whose `current_owner` answer is canned at
    /// construction. Used to drive `decide_bind` through both branches
    /// without spinning up a real D-Bus connection.
    struct MockBusProbe {
        owner: Option<String>,
    }

    impl NameOwnerProbe for MockBusProbe {
        fn current_owner(&self, _name: &str) -> Option<String> {
            self.owner.clone()
        }
    }

    /// Step P3-3 DoD: when `tuned-ppd.service` (or any other process)
    /// already owns `net.hadess.PowerProfiles`, the shim must NOT
    /// attempt to claim the name. `decide_bind` returns
    /// `BindDecision::Skip` carrying the existing owner's unique name
    /// so the INFO log identifies which peer holds the lever.
    #[test]
    #[tracing_test::traced_test]
    fn detects_existing_owner_and_skips_name_claim() {
        let probe = MockBusProbe {
            owner: Some(":1.35".to_string()),
        };
        let decision = decide_bind(true, &probe);
        assert!(
            matches!(decision, BindDecision::Skip { ref owner } if owner == ":1.35"),
            "owned name must degrade to observer mode, got {decision:?}",
        );
        log_bind_decision(&decision);
        assert!(
            logs_contain("PPD name owned by :1.35"),
            "observer-mode INFO must name the existing owner",
        );
    }

    /// Step P3-3 DoD: when nothing owns `net.hadess.PowerProfiles`
    /// (PPD masked, tuned-ppd inactive), `decide_bind` returns
    /// `BindDecision::Claim` so `spawn_system_bus_shim` proceeds to
    /// bind the well-known name.
    #[test]
    fn claims_name_when_not_owned() {
        let probe = MockBusProbe { owner: None };
        let decision = decide_bind(true, &probe);
        assert!(
            matches!(decision, BindDecision::Claim),
            "vacant name must result in Claim, got {decision:?}",
        );
    }

    /// `bind_name = false` (the `--with-ppd` operator opt-out) must
    /// short-circuit the probe — the operator explicitly asked PPD to
    /// keep the lever, so we never query the bus.
    #[test]
    fn with_ppd_opt_out_short_circuits_probe() {
        let probe = MockBusProbe { owner: None };
        let decision = decide_bind(false, &probe);
        assert!(matches!(decision, BindDecision::Skip { .. }));
    }
}
