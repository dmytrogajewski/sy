//! sy.target must be installable so the user manager pulls it up at
//! login. Without `[Install] WantedBy=...` the unit is `linked` but
//! `disabled`: nothing activates it and the whole "memory plane"
//! (sy-agentd, sy-knowledge, sy-qdrant, sy-aiplane workers) stays
//! down after reboot — they're symlinked into `sy.target.wants/` but
//! `sy.target` itself never starts.
//!
//! `PartOf=graphical-session.target` only propagates STOP, not START,
//! so it does not substitute for `WantedBy=`.

use std::path::Path;

const SY_TARGET: &str = "configs/systemd/user/sy.target";

#[test]
fn sy_target_has_install_section_with_wantedby() {
    let body = std::fs::read_to_string(SY_TARGET)
        .unwrap_or_else(|e| panic!("read {SY_TARGET}: {e}"));
    assert!(
        Path::new(SY_TARGET).exists(),
        "{SY_TARGET} must exist",
    );

    let mut in_install = false;
    let mut saw_wantedby = false;
    let mut wantedby_value: Option<String> = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed == "[Install]" {
            in_install = true;
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_install = false;
            continue;
        }
        if in_install {
            if let Some(rest) = trimmed.strip_prefix("WantedBy=") {
                saw_wantedby = true;
                wantedby_value = Some(rest.to_string());
            }
        }
    }

    assert!(
        saw_wantedby,
        "sy.target is missing `[Install] WantedBy=…`. Without it \
         `systemctl --user enable sy.target` cannot install a \
         `default.target.wants/sy.target` symlink, so the user \
         manager never pulls sy.target up at login and none of the \
         services symlinked under `sy.target.wants/` (sy-agentd, \
         sy-knowledge, sy-qdrant) activate after reboot.",
    );

    let target = wantedby_value.expect("wantedby_value set when saw_wantedby");
    assert!(
        target.split_ascii_whitespace().any(|t| t == "default.target"),
        "sy.target should be `WantedBy=default.target` so the user \
         manager pulls it up unconditionally at login (default.target \
         is the user-manager root, activated by user@.service). Got: \
         `WantedBy={target}`",
    );
}
