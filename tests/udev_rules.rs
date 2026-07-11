//! BUG-20260712-0111 guard: the shipped drm iGPU permission-grant udev
//! rule MUST match the AMD vendor ID with the parent-walking `ATTRS{vendor}`
//! operator, not the same-device `ATTR{vendor}`.
//!
//! `vendor` is a PCI attribute exposed on the drm node's PARENT device
//! (`/sys/class/drm/cardN/device/vendor`), NOT on the drm class node
//! itself. udev's `ATTR{...}` only reads attributes on the event device;
//! matching `ATTR{vendor}=="0x1002"` on a `SUBSYSTEM=="drm"` event can
//! therefore NEVER be true, so the rule never fires and
//! `power_dpm_force_performance_level` stays `0644 root:root` after boot —
//! the iGPU actuator then fails ~1 Hz with `actuator failed lever=igpu`.
//! `ATTRS{...}` walks the parent chain and matches the PCI vendor.

const UDEV_RULES: &str = include_str!("../configs/udev/rules.d/99-sy-power.rules");

/// The AMD PCI vendor ID the rule keys on.
const AMD_VENDOR: &str = "vendor}==\"0x1002\"";

#[test]
fn drm_vendor_match_uses_parent_walking_attrs() {
    // Only look at actual rule lines, not the header comment block.
    let rule_lines: Vec<&str> = UDEV_RULES
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    // The vendor match must appear on a real rule line, and it must use
    // the parent-walking `ATTRS{vendor}` form. A bare `ATTR{vendor}`
    // (same-device) can never match on a drm event and is the exact
    // BUG-20260712-0111 regression.
    let uses_attr_singular = rule_lines
        .iter()
        .any(|l| l.contains(&format!("ATTR{{{AMD_VENDOR}")) && !l.contains(&format!("ATTRS{{{AMD_VENDOR}")));
    assert!(
        !uses_attr_singular,
        "99-sy-power.rules matches the PCI `vendor` attribute with the \
         same-device `ATTR{{vendor}}` operator; `vendor` lives on the \
         drm node's PARENT PCI device, so this rule can never fire. Use \
         the parent-walking `ATTRS{{vendor}}` instead — see BUG-20260712-0111.",
    );

    let uses_attrs_plural = rule_lines
        .iter()
        .any(|l| l.contains(&format!("ATTRS{{{AMD_VENDOR}")));
    assert!(
        uses_attrs_plural,
        "99-sy-power.rules must key the drm iGPU permission grant on the \
         AMD PCI vendor via `ATTRS{{vendor}}==\"0x1002\"` (parent-walking); \
         no such match was found. See BUG-20260712-0111.",
    );
}
