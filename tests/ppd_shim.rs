//! Integration test for the Phase R7 / Step 36 PPD D-Bus shim.
//!
//! Gated behind `cfg(feature = "test-dbus")` because a real D-Bus
//! system bus + a polkit grant for the `net.hadess.PowerProfiles`
//! well-known name are required, neither of which exist on CI. To
//! run locally:
//!
//! ```sh
//! sudo systemctl mask power-profiles-daemon.service
//! sudo systemctl stop power-profiles-daemon.service
//! systemctl --user start sy-powerd.service
//! cargo test --features test-dbus --test ppd_shim
//! ```
//!
//! `sy` ships as a binary-only crate (no `lib.rs`), so this test does
//! NOT import `sy::power::ppd_shim` directly. Instead it walks the
//! live D-Bus surface the daemon exposes — exactly what GNOME's
//! quick-settings tile sees — and asserts the wire round-trip.
//!
//! Assumes a `sy power daemon` instance is running and bound to
//! `net.hadess.PowerProfiles` on the system bus. The test is
//! intentionally light: a single `SetActiveProfile("performance")`
//! call followed by a `Get ActiveProfile` readback. The pin-side
//! verification (the bandit arm flips to `build`) is covered by the
//! unit tests in `src/power/ppd_shim.rs`.

#![cfg(feature = "test-dbus")]

use zbus::blocking;

const PPD_DEST: &str = "net.hadess.PowerProfiles";
const PPD_OBJECT_PATH: &str = "/net/hadess/PowerProfiles";
const PPD_INTERFACE: &str = "net.hadess.PowerProfiles";

/// PPD `performance` is what GNOME quick-settings sends when the
/// operator clicks the third profile chip.
const PERF_PROFILE: &str = "performance";

/// DoD bullet 1: `gdbus introspect` returns the canonical interface,
/// asserted here via a property round-trip instead.
#[test]
fn active_profile_round_trips_over_system_bus() {
    let conn = blocking::Connection::system().expect("system bus");
    let props = blocking::fdo::PropertiesProxy::builder(&conn)
        .destination(PPD_DEST)
        .expect("destination")
        .path(PPD_OBJECT_PATH)
        .expect("path")
        .build()
        .expect("properties proxy");
    let iface = zbus::names::InterfaceName::try_from(PPD_INTERFACE).expect("iface name");

    props
        .set(
            iface.clone(),
            "ActiveProfile",
            zbus::zvariant::Value::from(PERF_PROFILE),
        )
        .expect("set ActiveProfile");

    let active = props
        .get(iface, "ActiveProfile")
        .expect("get ActiveProfile");
    let active_str: String = active.try_into().expect("string variant");
    assert_eq!(active_str, PERF_PROFILE);
}
