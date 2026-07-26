//! Integration tests for `sy file doctor` — Step 33 of the
//! [`sy-file-manager` roadmap][roadmap] / SPEC §3.3 item 19. Each test
//! synthesises a hermetic `XDG_CONFIG_HOME` / `XDG_STATE_HOME` fixture
//! and drives the [`doctor::file_doctor_checks`] runner against a
//! [`DoctorOpts`] pinned to those tempdirs, so the assertions never
//! read the host's real font registry, niri config, or systemd unit
//! dir.
//!
//! The `sy` package has no `lib.rs` (it is a `[[bin]]`); we pull the
//! doctor source in via `#[path]` (same pattern the journey e2e uses
//! for `manifest.rs` et al.). The doctor module's only `crate::` ref
//! is `crate::plugin::registry::discover()` inside
//! `discover_plugin_ids_via_registry`, which the tests never exercise
//! (they pin `discovered_plugin_ids` to a fixture vec).
//!
//! [roadmap]: ../specs/roadmaps/sy-file-manager/ROADMAP.md
#[path = "../src/file/doctor.rs"]
mod doctor;

// `doctor.rs`'s `discover_plugin_ids_via_registry` references
// `crate::plugin::registry`. The tests never reach that branch (we
// always pin `DoctorOpts.discovered_plugin_ids`), but the code path
// has to compile. Stub it out so the integration-test binary links.
#[allow(dead_code)]
mod plugin {
    pub(crate) mod registry {
        pub struct Registry;
        impl Registry {
            pub fn plugin_ids(&self) -> std::iter::Empty<PluginId> {
                std::iter::empty()
            }
        }
        pub struct PluginId(pub String);
        impl PluginId {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        pub fn discover() -> anyhow::Result<Registry> {
            Ok(Registry)
        }
    }
}

use std::fs;
use std::os::unix::net::UnixListener;
use std::path::Path;

use crate::doctor::{
    exit_code_for, file_doctor_checks, render_human, render_json, CheckStatus, DoctorOpts,
    SCHEMA_DOCTOR,
};

/// Build a minimal niri config that binds `Mod+E` / `Mod+Shift+E` /
/// `Mod+Slash` to `sy file`. Mirrors the productivised output Step 34
/// will write via `sy apply`.
const NIRI_CONFIG_HAPPY: &str = r#"
input { }

binds {
    Mod+E { spawn "sy" "file" "~"; }
    Mod+Shift+E { spawn "sy" "file"; }
    Mod+Slash { spawn "sy" "file" "~"; }
}
"#;

/// Niri config where `Mod+E` is bound to `swaylock` (a third-party
/// rebind silently shadowing `sy file`). Used by the collision check.
const NIRI_CONFIG_COLLISION: &str = r#"
binds {
    Mod+E { spawn "swaylock"; }
    Mod+Shift+E { spawn "sy" "file"; }
    Mod+Slash { spawn "sy" "file" "~"; }
}
"#;

/// Canary plugin id list returned by the happy-path fixture's
/// `discovered_plugin_ids`. Mirrors what `registry::discover()` would
/// return on a freshly-applied host: the Step 12 canary plus a
/// sentinel third-party plugin so the count check isn't vacuous.
fn happy_plugin_ids() -> Vec<String> {
    vec!["sy-plugin-md".to_string(), "sy-plugin-fake".to_string()]
}

/// File body for the synthetic font file. Just needs to exist with the
/// right name — the doctor probe is filesystem-level, not a font parse.
const FONT_FILE_BODY: &[u8] = b"";

/// Synthesise the JetBrainsMono Nerd Font marker by dropping a file
/// whose name matches the freedesktop convention into the fonts dir.
fn plant_jetbrainsmono(fonts_dir: &Path) {
    fs::create_dir_all(fonts_dir).expect("mkdir fonts_dir");
    fs::write(
        fonts_dir.join("JetBrainsMonoNerdFont-Regular.ttf"),
        FONT_FILE_BODY,
    )
    .expect("write font fixture");
}

/// Drop the productivised `sy-file.service` + `sy-file.socket` files
/// into the synthetic systemd unit dir.
fn plant_systemd_units(unit_dir: &Path) {
    fs::create_dir_all(unit_dir).expect("mkdir unit_dir");
    fs::write(unit_dir.join("sy-file.service"), "[Unit]\n").expect("write service");
    fs::write(unit_dir.join("sy-file.socket"), "[Socket]\n").expect("write socket");
}

/// Spin up a UDS listener at `sock` so the daemon-reachable probe sees
/// a connectable socket. Returns the listener so the test can hold it
/// until the assertion completes.
fn spawn_fake_daemon(sock: &Path) -> UnixListener {
    if let Some(parent) = sock.parent() {
        fs::create_dir_all(parent).expect("mkdir parent");
    }
    UnixListener::bind(sock).expect("bind fake daemon")
}

/// Build a hermetic `DoctorOpts` mirroring `sy apply` output (niri
/// config, systemd unit dir, fonts dir, bookmarks state dir, fake
/// daemon socket, canary plugin id list). Holds the tempdir + listener
/// so the caller doesn't have to.
struct HappyFixture {
    _tmp: tempfile::TempDir,
    _daemon: UnixListener,
    opts: DoctorOpts,
}

fn happy_fixture() -> HappyFixture {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    let niri_config = root.join("niri").join("config.kdl");
    fs::create_dir_all(niri_config.parent().expect("parent")).expect("mkdir niri dir");
    fs::write(&niri_config, NIRI_CONFIG_HAPPY).expect("write niri config");
    let fonts_dir = root.join("fonts");
    plant_jetbrainsmono(&fonts_dir);
    let unit_dir = root.join("systemd").join("user");
    plant_systemd_units(&unit_dir);
    let bookmarks_dir = root.join("state").join("sy").join("file");
    let sock_path = root.join("sy-file.sock");
    let daemon = spawn_fake_daemon(&sock_path);
    let opts = DoctorOpts {
        sock_path: Some(sock_path),
        fonts_dir: Some(fonts_dir),
        niri_config: Some(niri_config),
        systemd_user_unit_dir: Some(unit_dir),
        bookmarks_dir: Some(bookmarks_dir),
        discovered_plugin_ids: Some(happy_plugin_ids()),
    };
    HappyFixture {
        _tmp: tmp,
        _daemon: daemon,
        opts,
    }
}

/// Step 33 DoD bullet 1 — `happy_path_all_green`. Every probe Ok →
/// exit code 0 → JSON envelope `status = "ok"`.
#[test]
fn happy_path_all_green() {
    let fx = happy_fixture();
    let checks = file_doctor_checks(fx.opts);
    for c in &checks {
        assert_eq!(
            c.status,
            CheckStatus::Ok,
            "check {:?} expected Ok but got {:?}: detail={}, fix={:?}",
            c.name,
            c.status,
            c.detail,
            c.fix_hint,
        );
    }
    assert_eq!(exit_code_for(&checks), 0);
    let v = render_json(&checks);
    assert_eq!(v["schema"], SCHEMA_DOCTOR);
    assert_eq!(v["status"], "ok");
}

/// Step 33 DoD bullet 2 — `detects_missing_jetbrainsmono_nerdfont`.
/// Fonts dir exists but has no Nerd Font file → Fail with the
/// productivised fix-hint `dnf install jetbrainsmono-nerd-fonts`.
#[test]
fn detects_missing_jetbrainsmono_nerdfont() {
    let fx = happy_fixture();
    let empty_dir = fx._tmp.path().join("empty-fonts");
    fs::create_dir_all(&empty_dir).expect("mkdir empty fonts");
    let opts = DoctorOpts {
        fonts_dir: Some(empty_dir),
        ..fx.opts
    };
    let checks = file_doctor_checks(opts);
    let fonts_check = checks
        .iter()
        .find(|c| c.name == "file.fonts.jetbrainsmono_nerd")
        .expect("fonts check must surface");
    assert_eq!(
        fonts_check.status,
        CheckStatus::Fail,
        "fonts probe must Fail on missing JetBrainsMono Nerd Font: {fonts_check:?}",
    );
    let fix = fonts_check.fix_hint.clone().unwrap_or_default();
    assert!(
        fix.contains("jetbrainsmono-nerd-fonts"),
        "fix-hint must name the productivised package, got {fix:?}",
    );
}

/// Step 33 DoD bullet 3 — `detects_niri_keybind_collision`. A `Mod+E`
/// bind exists but spawns `swaylock` (not `sy file`); the probe must
/// surface the collision with the third-party target named in the
/// detail string.
#[test]
fn detects_niri_keybind_collision() {
    let fx = happy_fixture();
    let collision_path = fx._tmp.path().join("collision.kdl");
    fs::write(&collision_path, NIRI_CONFIG_COLLISION).expect("write collision config");
    let opts = DoctorOpts {
        niri_config: Some(collision_path),
        ..fx.opts
    };
    let checks = file_doctor_checks(opts);
    let niri_check = checks
        .iter()
        .find(|c| c.name == "file.niri.binds")
        .expect("niri check must surface");
    assert_eq!(
        niri_check.status,
        CheckStatus::Fail,
        "niri probe must Fail on collision: {niri_check:?}",
    );
    assert!(
        niri_check.detail.contains("Mod+E"),
        "detail must name the colliding bind, got {}",
        niri_check.detail
    );
    assert!(
        niri_check.detail.contains("swaylock"),
        "detail must name the third-party spawn target, got {}",
        niri_check.detail
    );
}

/// Step 33 DoD bullet 4 — `detects_unhealthy_plugin`. An empty
/// registry → Fail with a detail string naming the diagnostic. The
/// canary `sy-plugin-md` is the journey-J3 hover preview path's
/// chokepoint: a missing canary means the first hover surfaces
/// nothing, which is exactly what doctor must catch pre-flight.
#[test]
fn detects_unhealthy_plugin() {
    let fx = happy_fixture();
    let opts = DoctorOpts {
        discovered_plugin_ids: Some(Vec::new()),
        ..fx.opts
    };
    let checks = file_doctor_checks(opts);
    let plugin_check = checks
        .iter()
        .find(|c| c.name == "file.plugins.registry")
        .expect("plugin check must surface");
    assert!(
        plugin_check.status != CheckStatus::Ok,
        "plugin probe must surface non-Ok on empty registry: {plugin_check:?}",
    );
    let detail_lower = plugin_check.detail.to_lowercase();
    assert!(
        detail_lower.contains("plugin"),
        "plugin probe detail must name the diagnostic, got {}",
        plugin_check.detail
    );
}

/// Companion smoke for `render_human` — the human renderer is the
/// `print!` path the bin's `run_doctor` rides on. Exercising it here
/// keeps the `#[path]`-imported doctor module's full public surface
/// reachable from the integration-test binary (otherwise the
/// dead-code pass would flag `render_human` since the test binary
/// doesn't reach `run_doctor`).
#[test]
fn render_human_includes_summary_footer() {
    let fx = happy_fixture();
    let checks = file_doctor_checks(fx.opts);
    let body = render_human(&checks);
    assert!(
        body.contains("sy file doctor:"),
        "human renderer must surface the summary footer: {body}"
    );
}

/// Step 33 DoD bullet 5 — `json_schema_stable`. The `render_json`
/// envelope must carry the documented `schema` / `status` / `checks`
/// shape. Pinned so a future refactor that re-orders the fields or
/// re-cases the status enum breaks here before it breaks operators.
#[test]
fn json_schema_stable() {
    let fx = happy_fixture();
    let checks = file_doctor_checks(fx.opts);
    let v = render_json(&checks);
    assert_eq!(
        v["schema"].as_str(),
        Some("sy.file.doctor/v1"),
        "schema marker must pin /v1: {v}"
    );
    let status = v["status"].as_str().unwrap_or_default();
    assert!(
        matches!(status, "ok" | "warn" | "fail"),
        "top-level status must be ok/warn/fail, got {status:?}",
    );
    let checks_arr = v["checks"].as_array().expect("checks array");
    assert!(
        !checks_arr.is_empty(),
        "checks must surface at least one row"
    );
    for c in checks_arr {
        assert!(c["name"].is_string(), "each row must carry a name: {c}");
        let row_status = c["status"].as_str().unwrap_or_default();
        assert!(
            matches!(row_status, "ok" | "warn" | "fail"),
            "row status must be ok/warn/fail, got {row_status:?} in {c}",
        );
        assert!(c["detail"].is_string(), "each row must carry a detail: {c}");
        // `fix_hint` is optional — absent on Ok rows by convention,
        // present on Warn/Fail. When present it must be a string per
        // the `skip_serializing_if` contract.
        if let Some(fix) = c.get("fix_hint") {
            assert!(
                fix.is_string(),
                "fix_hint must be a string when present: {c}"
            );
        }
    }
}
