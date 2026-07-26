//! `sy file doctor` — health probes for the `sy file` plane (SPEC §3.3
//! item 19; ROADMAP Step 33). The journey-J1 pre-flight: if doctor
//! lies, the user's first `Mod+E` silently breaks.
//!
//! Six probes run in stable order — daemon reachability, fonts, niri
//! keybinds, systemd units, bookmarks dir writability, plugin registry
//! reachability. Each surfaces a [`CheckStatus`] (`Ok | Warn | Fail`)
//! plus a one-line `detail` and an optional `fix_hint` so an operator
//! reading the human form gets a one-paste path to green.
//!
//! The [`DoctorOpts`] bundle is the test seam: every probe reads its
//! input from a field on `DoctorOpts` rather than the process env, so
//! `tests/sy_file_doctor.rs` can drive the whole surface against a
//! tempdir-based fixture without mutating `$XDG_CONFIG_HOME` /
//! `$XDG_STATE_HOME` for the test binary.
//!
//! Wire schema is `sy.file.doctor/v1`; the JSON shape is documented at
//! [`docs/reference/sy-file-doctor.md`][doc]. Schema is additive-only —
//! a new check appended at the end of the list does not bump the major.
//!
//! [doc]: ../../../docs/reference/sy-file-doctor.md

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

/// Wire-stable schema marker emitted on the `--json` envelope. Step 33
/// pins `/v1`; bumped only on a breaking shape change (additive
/// fields / new check rows do NOT bump the major).
pub const SCHEMA_DOCTOR: &str = "sy.file.doctor/v1";

/// Canary plugin id the registry-reachable check asserts is present.
/// Same productivised plugin Step 12 ships under
/// `configs/sy/plugins/sy-plugin-md/`.
pub const CANARY_PLUGIN_ID: &str = "sy-plugin-md";

/// Niri binds the journey-J1 keybinds must reference. The collision
/// check compares the bound action's spawn target against `sy file` to
/// detect a third-party rebind silently shadowing the productivised
/// dispatcher.
pub const REQUIRED_NIRI_BINDS: &[&str] = &["Mod+E", "Mod+Shift+E", "Mod+Slash"];

/// Per-check status. Kebab-cased on the wire so JSON consumers don't
/// have to know Rust's PascalCase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

impl CheckStatus {
    /// Kebab-case wire form ("ok" / "warn" / "fail").
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        }
    }
}

/// One probe outcome. Field order matches the documented
/// `sy.file.doctor/v1` schema; `name` is a stable dot-separated id, the
/// `fix_hint` is the one-paste path to green an operator pastes into a
/// shell.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<String>,
}

/// Probe-input bundle. Every field defaults to the process env when
/// `None`; tests pin each to a tempdir so the whole surface runs
/// hermetically.
#[derive(Debug, Clone, Default)]
pub struct DoctorOpts {
    /// `$SY_FILE_SOCK` override. `None` falls back to
    /// [`crate::file::cli::resolve_sock_path`].
    pub sock_path: Option<PathBuf>,
    /// Directory holding `fonts.conf` / `fontconfig` lookup target;
    /// when `Some`, the JetBrainsMono check walks this dir for any
    /// file whose name matches the Nerd Font naming convention. When
    /// `None`, the check shells out to `fc-list` (best-effort).
    pub fonts_dir: Option<PathBuf>,
    /// Path to a `niri/config.kdl` to parse for the journey-J1 binds.
    /// `None` falls back to `$XDG_CONFIG_HOME/niri/config.kdl`.
    pub niri_config: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME/systemd/user/` (or equivalent) to scan for
    /// `sy-file.service` / `sy-file.socket`. `None` defaults to the
    /// freedesktop default.
    pub systemd_user_unit_dir: Option<PathBuf>,
    /// `$XDG_STATE_HOME/sy/file/` for the bookmarks writability probe.
    /// `None` defaults to the freedesktop fallback.
    pub bookmarks_dir: Option<PathBuf>,
    /// Pre-discovered plugin id list (`Some`) bypasses the live
    /// `registry::discover()` call. The doctor probe asserts the
    /// canary `sy-plugin-md` (Step 12) appears in the list; tests pin
    /// the list at `[]` to exercise the "empty registry" failure
    /// branch without standing up a full discovery root.
    pub discovered_plugin_ids: Option<Vec<String>>,
}

/// Run every probe in stable order. The list shape pins the wire
/// contract — new rows appended at the end are additive (no schema
/// bump).
pub fn file_doctor_checks(opts: DoctorOpts) -> Vec<DoctorCheck> {
    vec![
        check_daemon(&opts),
        check_fonts(&opts),
        check_niri_binds(&opts),
        check_systemd_unit(&opts),
        check_bookmarks_writable(&opts),
        check_plugin_registry(&opts),
    ]
}

/// Render the `sy.file.doctor/v1` JSON envelope. Top-level `status` is
/// the worst-of-checks: any `Fail` → `fail`, else any `Warn` → `warn`,
/// else `ok`.
pub fn render_json(checks: &[DoctorCheck]) -> Value {
    let status = overall_status(checks);
    json!({
        "schema": SCHEMA_DOCTOR,
        "status": status.as_str(),
        "checks": checks,
    })
}

/// Human-readable rendering. One line per check with a status glyph
/// and the `fix_hint` indented underneath. Honours `NO_COLOR` (and a
/// non-TTY stdout via the caller's chosen renderer); the glyphs are
/// plain ASCII so no terminal capabilities are assumed.
pub fn render_human(checks: &[DoctorCheck]) -> String {
    let mut out = String::new();
    for c in checks {
        let tag = match c.status {
            CheckStatus::Ok => "OK  ",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        };
        out.push_str(&format!("{tag} {} — {}\n", c.name, c.detail));
        if let Some(fix) = &c.fix_hint {
            out.push_str(&format!("     fix: {fix}\n"));
        }
    }
    let overall = overall_status(checks);
    out.push_str(&format!("\nsy file doctor: {}\n", overall.as_str()));
    out
}

/// SPEC §4.7-style exit code: 0 all-pass, 1 any-fail, 2 warn-only.
/// Mirrors the `syauth::doctor_exit_code` pattern.
pub fn exit_code_for(checks: &[DoctorCheck]) -> i32 {
    match overall_status(checks) {
        CheckStatus::Ok => 0,
        CheckStatus::Fail => 1,
        CheckStatus::Warn => 2,
    }
}

/// Reduce a slice of check outcomes to a single envelope status.
fn overall_status(checks: &[DoctorCheck]) -> CheckStatus {
    if checks.iter().any(|c| c.status == CheckStatus::Fail) {
        CheckStatus::Fail
    } else if checks.iter().any(|c| c.status == CheckStatus::Warn) {
        CheckStatus::Warn
    } else {
        CheckStatus::Ok
    }
}

// ── probes ────────────────────────────────────────────────────────────

/// `file.daemon.reachable` — assert the daemon socket exists and a
/// blocking UDS connect succeeds. We avoid an `system.health`
/// round-trip so the doctor returns fast even if the daemon's queue
/// is hung.
fn check_daemon(opts: &DoctorOpts) -> DoctorCheck {
    let sock = opts.sock_path.clone().unwrap_or_else(default_sock_path);
    if UnixStream::connect(&sock).is_ok() {
        DoctorCheck {
            name: "file.daemon.reachable".into(),
            status: CheckStatus::Ok,
            detail: format!("daemon socket {} accepts connections", sock.display()),
            fix_hint: None,
        }
    } else {
        DoctorCheck {
            name: "file.daemon.reachable".into(),
            status: CheckStatus::Fail,
            detail: format!("daemon socket {} not accepting", sock.display()),
            fix_hint: Some("systemctl --user start sy-file.socket".into()),
        }
    }
}

/// Env-var override for the fonts probe — when set, the doctor walks
/// this directory instead of shelling out to `fc-list`. Lets the
/// productivised e2e (and any agent invocation against a fixture
/// `XDG_DATA_HOME`) pin the probe without touching the host's font
/// registry.
pub const FONTS_DIR_ENV: &str = "SY_FILE_FONTS_DIR";

/// `file.fonts.jetbrainsmono_nerd` — confirm the JetBrainsMono Nerd
/// Font is reachable. With `opts.fonts_dir` set, we walk that dir for a
/// file matching the Nerd-Font naming convention. Without it, we shell
/// out to `fc-list` (best-effort; if `fc-list` is absent the check
/// surfaces a `Warn`).
fn check_fonts(opts: &DoctorOpts) -> DoctorCheck {
    let needle_substr = "JetBrainsMono";
    let nerd_marker = "Nerd";
    let env_dir = std::env::var_os(FONTS_DIR_ENV).map(PathBuf::from);
    let dir_override = opts.fonts_dir.clone().or(env_dir);
    if let Some(dir) = dir_override.as_ref() {
        let hit = walk_for_font(dir, needle_substr, nerd_marker);
        return match hit {
            Some(path) => DoctorCheck {
                name: "file.fonts.jetbrainsmono_nerd".into(),
                status: CheckStatus::Ok,
                detail: format!("found {}", path.display()),
                fix_hint: None,
            },
            None => DoctorCheck {
                name: "file.fonts.jetbrainsmono_nerd".into(),
                status: CheckStatus::Fail,
                detail: format!("no JetBrainsMono Nerd Font under {}", dir.display()),
                fix_hint: Some("dnf install jetbrainsmono-nerd-fonts".into()),
            },
        };
    }
    // Live host: shell out to fc-list. Tolerant of fc-list missing
    // (degrades to Warn).
    match std::process::Command::new("fc-list").output() {
        Ok(out) if out.status.success() => {
            let body = String::from_utf8_lossy(&out.stdout);
            let has = body
                .lines()
                .any(|l| l.contains(needle_substr) && l.contains(nerd_marker));
            if has {
                DoctorCheck {
                    name: "file.fonts.jetbrainsmono_nerd".into(),
                    status: CheckStatus::Ok,
                    detail: "fc-list reports JetBrainsMono Nerd Font present".into(),
                    fix_hint: None,
                }
            } else {
                DoctorCheck {
                    name: "file.fonts.jetbrainsmono_nerd".into(),
                    status: CheckStatus::Fail,
                    detail: "fc-list does not list JetBrainsMono Nerd Font".into(),
                    fix_hint: Some("dnf install jetbrainsmono-nerd-fonts".into()),
                }
            }
        }
        _ => DoctorCheck {
            name: "file.fonts.jetbrainsmono_nerd".into(),
            status: CheckStatus::Warn,
            detail: "fc-list missing; cannot probe fonts on this host".into(),
            fix_hint: Some("install fontconfig + jetbrainsmono-nerd-fonts".into()),
        },
    }
}

/// Walk `dir` looking for any file whose name contains both
/// `JetBrainsMono` and `Nerd`. Returns the first hit (depth-1 only —
/// freedesktop convention drops fonts directly under `fonts/`).
fn walk_for_font(dir: &Path, needle: &str, marker: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.contains(needle) && name.contains(marker) {
            return Some(path);
        }
    }
    None
}

/// `file.niri.binds` — parse `config.kdl` and assert the three
/// journey-J1 binds (`Mod+E`, `Mod+Shift+E`, `Mod+Slash`) reference
/// `sy file`. Detect collisions — if a bind exists but its spawn
/// target is something other than `sy file`, surface a `Fail` so an
/// operator knows their `Mod+E` is silently broken.
fn check_niri_binds(opts: &DoctorOpts) -> DoctorCheck {
    let path = match opts.niri_config.as_ref() {
        Some(p) => p.clone(),
        None => default_niri_config_path(),
    };
    let body = match fs::read_to_string(&path) {
        Ok(b) => b,
        Err(_) => {
            return DoctorCheck {
                name: "file.niri.binds".into(),
                status: CheckStatus::Fail,
                detail: format!("niri config not found at {}", path.display()),
                fix_hint: Some("sy apply".into()),
            };
        }
    };
    let mut missing: Vec<&str> = Vec::new();
    let mut collisions: Vec<(String, String)> = Vec::new();
    for bind in REQUIRED_NIRI_BINDS {
        match find_bind_target(&body, bind) {
            None => missing.push(bind),
            Some(target) => {
                if !target_dispatches_sy_file(&target) {
                    collisions.push(((*bind).to_string(), target));
                }
            }
        }
    }
    if !collisions.is_empty() {
        let (bind, target) = &collisions[0];
        return DoctorCheck {
            name: "file.niri.binds".into(),
            status: CheckStatus::Fail,
            detail: format!("{bind} collides with {target}"),
            fix_hint: Some("sy apply".into()),
        };
    }
    if !missing.is_empty() {
        return DoctorCheck {
            name: "file.niri.binds".into(),
            status: CheckStatus::Fail,
            detail: format!("missing niri binds: {}", missing.join(", ")),
            fix_hint: Some("sy apply".into()),
        };
    }
    DoctorCheck {
        name: "file.niri.binds".into(),
        status: CheckStatus::Ok,
        detail: "Mod+E / Mod+Shift+E / Mod+Slash all dispatch to `sy file`".into(),
        fix_hint: None,
    }
}

/// Find the bind target string for `bind_name` in a niri KDL body. The
/// parser is intentionally line-oriented (no real KDL parse): the niri
/// `binds {}` block uses `<key> { <action>; }` one-line shape on every
/// productivised entry, so a substring extract is enough. Returns the
/// inner action string (everything between `{` and `}`).
fn find_bind_target(body: &str, bind_name: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        // Match `<bind_name>{whitespace-or-`{`}` so `Mod+E` does not
        // match `Mod+Shift+E` and vice versa.
        let after = match trimmed.strip_prefix(bind_name) {
            Some(rest) => rest,
            None => continue,
        };
        if !after.starts_with(|c: char| c.is_whitespace() || c == '{') {
            continue;
        }
        // Extract everything between `{` and `}` on the same line.
        let (Some(open), Some(close)) = (trimmed.find('{'), trimmed.rfind('}')) else {
            continue;
        };
        if close <= open {
            continue;
        }
        let inner = trimmed[open + 1..close].trim().to_string();
        return Some(inner);
    }
    None
}

/// Resolve `$SY_FILE_SOCK` (preferred) or the freedesktop
/// `$XDG_RUNTIME_DIR/sy-file.sock` fallback for the daemon probe. Same
/// shape as `crate::file::cli::resolve_sock_path` but local here so
/// the doctor module stays decoupled from the CLI module — the
/// integration test imports doctor.rs via `#[path]` without dragging
/// the CLI's tokio runtime in.
fn default_sock_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SY_FILE_SOCK") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("sy-file.sock");
        }
    }
    // Same uid-based fallback the CLI uses.
    // SAFETY: getuid() is async-signal-safe and has no preconditions.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/sy-file.sock"))
}

/// `true` when the niri spawn action references `sy file` (either as
/// the inline `spawn "sy file"`, the multi-arg `spawn "sy" "file"`
/// form, or the productivised `spawn "{{ home }}/.local/bin/sy"
/// "file"` form Step 34 lands). The check is intentionally loose —
/// we don't enforce a specific argv shape, only that the dispatched
/// binary is some `sy` (bare or `/bin/sy` suffix) AND `file` is
/// somewhere in the arg list.
fn target_dispatches_sy_file(target: &str) -> bool {
    let has_sy = target.contains("\"sy\"")
        || target.contains("\"sy ")
        || target.contains(" sy ")
        || target.contains("/bin/sy\"")
        || target.contains("/bin/sy ");
    let has_file = target.contains("\"file\"")
        || target.contains(" file\"")
        || target.contains(" file ")
        || target.contains("\"file ");
    has_sy && has_file
}

/// Resolve the freedesktop default for `niri/config.kdl`.
fn default_niri_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join("niri").join("config.kdl")
}

/// `file.systemd.unit_installed` — assert `sy-file.service` and
/// `sy-file.socket` are present under the user's systemd unit dir.
/// We don't probe `systemctl --user is-enabled` because the test
/// fixture won't have a live systemd; presence on disk is the
/// productivised `sy apply` output's load-bearing artifact.
fn check_systemd_unit(opts: &DoctorOpts) -> DoctorCheck {
    let dir = match opts.systemd_user_unit_dir.as_ref() {
        Some(p) => p.clone(),
        None => default_systemd_user_unit_dir(),
    };
    let service = dir.join("sy-file.service");
    let socket = dir.join("sy-file.socket");
    let missing: Vec<&Path> = [service.as_path(), socket.as_path()]
        .into_iter()
        .filter(|p| !p.exists())
        .collect();
    if missing.is_empty() {
        DoctorCheck {
            name: "file.systemd.unit_installed".into(),
            status: CheckStatus::Ok,
            detail: format!(
                "sy-file.service + sy-file.socket present under {}",
                dir.display()
            ),
            fix_hint: None,
        }
    } else {
        let names: Vec<String> = missing.iter().map(|p| p.display().to_string()).collect();
        DoctorCheck {
            name: "file.systemd.unit_installed".into(),
            status: CheckStatus::Fail,
            detail: format!("missing unit files: {}", names.join(", ")),
            fix_hint: Some("sy apply".into()),
        }
    }
}

/// Resolve `$XDG_CONFIG_HOME/systemd/user/` (or freedesktop fallback).
fn default_systemd_user_unit_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join("systemd").join("user")
}

/// `file.bookmarks.writable` — assert `$XDG_STATE_HOME/sy/file/`
/// exists (or can be created) and is writable. Without it the
/// bookmarks daemon (Step 31) can't persist `b<key>` pins.
fn check_bookmarks_writable(opts: &DoctorOpts) -> DoctorCheck {
    let dir = match opts.bookmarks_dir.as_ref() {
        Some(p) => p.clone(),
        None => default_bookmarks_dir(),
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        return DoctorCheck {
            name: "file.bookmarks.writable".into(),
            status: CheckStatus::Fail,
            detail: format!("create_dir_all {}: {}", dir.display(), e),
            fix_hint: Some("verify $XDG_STATE_HOME is writable for the current user".into()),
        };
    }
    let probe = dir.join(".sy-file-doctor-probe");
    match fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            DoctorCheck {
                name: "file.bookmarks.writable".into(),
                status: CheckStatus::Ok,
                detail: format!("{} writable", dir.display()),
                fix_hint: None,
            }
        }
        Err(e) => DoctorCheck {
            name: "file.bookmarks.writable".into(),
            status: CheckStatus::Fail,
            detail: format!("write probe {}: {}", probe.display(), e),
            fix_hint: Some("verify the directory is writable by the current user".into()),
        },
    }
}

/// Resolve `$XDG_STATE_HOME/sy/file/` (or freedesktop fallback).
fn default_bookmarks_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("state")
        });
    base.join("sy").join("file")
}

/// `file.plugins.registry` — assert the plugin registry discovers at
/// least the canary `sy-plugin-md` (Step 12). When the caller supplies
/// `discovered_plugin_ids`, use that directly; otherwise
/// [`discover_plugin_ids_via_registry`] dials the live
/// `crate::plugin::registry::discover()` surface.
fn check_plugin_registry(opts: &DoctorOpts) -> DoctorCheck {
    let owned;
    let ids: &[String] = match opts.discovered_plugin_ids.as_deref() {
        Some(slice) => slice,
        None => match discover_plugin_ids_via_registry() {
            Ok(v) => {
                owned = v;
                owned.as_slice()
            }
            Err(e) => {
                return DoctorCheck {
                    name: "file.plugins.registry".into(),
                    status: CheckStatus::Fail,
                    detail: format!("registry discover failed: {e}"),
                    fix_hint: Some("sy plugin doctor --json".into()),
                };
            }
        },
    };
    if ids.is_empty() {
        return DoctorCheck {
            name: "file.plugins.registry".into(),
            status: CheckStatus::Fail,
            detail: "registry discovered no plugins".into(),
            fix_hint: Some("sy plugin install ./crates/sy-plugin-md".into()),
        };
    }
    if !ids.iter().any(|id| id == CANARY_PLUGIN_ID) {
        return DoctorCheck {
            name: "file.plugins.registry".into(),
            status: CheckStatus::Warn,
            detail: format!("canary plugin {CANARY_PLUGIN_ID:?} absent; found: {ids:?}"),
            fix_hint: Some("sy plugin install ./crates/sy-plugin-md".into()),
        };
    }
    DoctorCheck {
        name: "file.plugins.registry".into(),
        status: CheckStatus::Ok,
        detail: format!(
            "{} plugin(s) discovered including {}",
            ids.len(),
            CANARY_PLUGIN_ID
        ),
        fix_hint: None,
    }
}

/// Live-host registry discovery: dial
/// `crate::plugin::registry::discover()` and surface the plugin id
/// list. Separated from [`check_plugin_registry`] so the integration
/// test (`tests/sy_file_doctor.rs`) can pin the id list via
/// `DoctorOpts.discovered_plugin_ids` without `#[path]`-importing the
/// full registry module.
fn discover_plugin_ids_via_registry() -> Result<Vec<String>, String> {
    let reg = crate::plugin::registry::discover().map_err(|e| format!("{e:#}"))?;
    Ok(reg.plugin_ids().map(|id| id.as_str().to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overall_status_is_worst_of_checks() {
        let mut cs = vec![DoctorCheck {
            name: "a".into(),
            status: CheckStatus::Ok,
            detail: "ok".into(),
            fix_hint: None,
        }];
        assert_eq!(overall_status(&cs), CheckStatus::Ok);
        cs.push(DoctorCheck {
            name: "b".into(),
            status: CheckStatus::Warn,
            detail: "warn".into(),
            fix_hint: None,
        });
        assert_eq!(overall_status(&cs), CheckStatus::Warn);
        cs.push(DoctorCheck {
            name: "c".into(),
            status: CheckStatus::Fail,
            detail: "fail".into(),
            fix_hint: None,
        });
        assert_eq!(overall_status(&cs), CheckStatus::Fail);
    }

    #[test]
    fn exit_code_mirrors_overall_status() {
        let ok = vec![DoctorCheck {
            name: "a".into(),
            status: CheckStatus::Ok,
            detail: String::new(),
            fix_hint: None,
        }];
        let warn = vec![DoctorCheck {
            name: "a".into(),
            status: CheckStatus::Warn,
            detail: String::new(),
            fix_hint: None,
        }];
        let fail = vec![DoctorCheck {
            name: "a".into(),
            status: CheckStatus::Fail,
            detail: String::new(),
            fix_hint: None,
        }];
        assert_eq!(exit_code_for(&ok), 0);
        assert_eq!(exit_code_for(&warn), 2);
        assert_eq!(exit_code_for(&fail), 1);
    }

    #[test]
    fn render_json_carries_schema_marker() {
        let checks = vec![DoctorCheck {
            name: "x".into(),
            status: CheckStatus::Ok,
            detail: "ok".into(),
            fix_hint: None,
        }];
        let v = render_json(&checks);
        assert_eq!(v["schema"], SCHEMA_DOCTOR);
        assert_eq!(v["status"], "ok");
        assert!(v["checks"].is_array());
    }

    #[test]
    fn find_bind_target_extracts_action_body() {
        let body = r#"
binds {
    Mod+E { spawn "sy" "file"; }
    Mod+Slash { spawn "sy" "file" "~"; }
}
"#;
        assert_eq!(
            find_bind_target(body, "Mod+E").as_deref(),
            Some("spawn \"sy\" \"file\";")
        );
        // `Mod+E` does not accidentally match `Mod+Shift+E`.
        assert_eq!(find_bind_target(body, "Mod+Shift+E"), None);
    }
}
