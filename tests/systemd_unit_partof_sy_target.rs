//! arch-supervision Step 1 DoD: every user-level `sy-*.service`
//! declares `PartOf=sy.target` in its `[Unit]` section so that
//! `systemctl --user stop sy.target` tears the whole desktop AI
//! plane down atomically (SPEC §4.5 group root).
//!
//! Exemptions:
//! - `sy.target` itself (it _is_ the group root).
//! - `sy-knowledge.socket` (socket units are bound by `Requires=`
//!   from the corresponding `.service`; SPEC §4.5 shape only
//!   places `PartOf=sy.target` on services, not on sockets).
//!
//! The check is intentionally substring-based, not section-aware:
//! anywhere in the file the directive `PartOf=sy.target` appears
//! satisfies the assertion. `systemd-analyze --user verify`
//! (covered by `systemd_unit_files_parse.rs`) catches malformed
//! sections.

use std::path::PathBuf;

const UNIT_DIR: &str = "configs/systemd/user";
const REQUIRED_DIRECTIVE: &str = "PartOf=sy.target";

fn unit_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(UNIT_DIR).unwrap_or_else(|e| panic!("read {UNIT_DIR}: {e}"));
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

fn is_exempt(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    // `sy.target` is the group root; `*.socket` units are bound
    // into the group via the matching `.service`'s `Requires=`.
    name == "sy.target" || name.ends_with(".socket")
}

#[test]
fn every_sy_service_declares_partof_sy_target() {
    let units = unit_files();
    assert!(
        !units.is_empty(),
        "no user-level unit files found under {UNIT_DIR}",
    );

    let mut missing = Vec::new();
    let mut checked = 0usize;
    for unit in &units {
        if is_exempt(unit) {
            continue;
        }
        checked += 1;
        let body = std::fs::read_to_string(unit)
            .unwrap_or_else(|e| panic!("read {}: {e}", unit.display()));
        let has_directive = body.lines().any(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#')
                && !trimmed.starts_with(';')
                && line.contains(REQUIRED_DIRECTIVE)
        });
        if !has_directive {
            missing.push(unit.display().to_string());
        }
    }

    assert!(
        checked > 0,
        "no non-exempt unit files found under {UNIT_DIR}",
    );
    assert!(
        missing.is_empty(),
        "units missing `{REQUIRED_DIRECTIVE}`:\n  {}",
        missing.join("\n  "),
    );
}
