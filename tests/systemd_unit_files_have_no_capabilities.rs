//! arch-supervision Step 1 DoD: user-mode systemd units cannot
//! grant `AmbientCapabilities=` or `CapabilityBoundingSet=`. systemd
//! silently ignores these in `--user` scopes (`man systemd.exec`,
//! "Implicitly set capabilities"), so a regression here would
//! produce units that look hardened but aren't.
//!
//! Walks every unit file under `configs/systemd/user/` and asserts
//! neither directive appears in any of them.

use std::path::PathBuf;

const UNIT_DIR: &str = "configs/systemd/user";
const FORBIDDEN_DIRECTIVES: &[&str] = &["AmbientCapabilities=", "CapabilityBoundingSet="];

fn user_unit_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let dir = std::path::Path::new(UNIT_DIR);
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {UNIT_DIR}: {e}"));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if matches!(ext, "service" | "socket" | "target") {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn no_user_unit_declares_ambient_capabilities() {
    let units = user_unit_files();
    assert!(
        !units.is_empty(),
        "no user-level unit files found under {UNIT_DIR}",
    );

    let mut offenders = Vec::new();
    for unit in &units {
        let body = std::fs::read_to_string(unit)
            .unwrap_or_else(|e| panic!("read {}: {e}", unit.display()));
        for (lineno, line) in body.lines().enumerate() {
            // Skip comments — the head comments in `sy-knowledge.service`
            // legitimately mention `CAP_IPC_LOCK` to explain why it
            // was dropped.
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }
            for needle in FORBIDDEN_DIRECTIVES {
                if line.contains(needle) {
                    offenders.push(format!(
                        "{}:{}: forbidden directive `{}` (user-mode systemd ignores it)",
                        unit.display(),
                        lineno + 1,
                        needle,
                    ));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "user-level units must not grant capabilities:\n{}",
        offenders.join("\n"),
    );
}
