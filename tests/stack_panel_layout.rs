//! The panel owns its reservation; Waybar must map before it on every start.

#[test]
fn panel_reservation_is_not_duplicated_by_a_niri_strut() {
    let config = include_str!("../configs/niri/config.kdl");
    let struts = config
        .split("struts {")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    assert!(struts.lines().any(|line| line.trim() == "right  0"));
}

#[test]
fn waybar_restarts_reorder_the_stack_panel() {
    let unit = include_str!("../configs/systemd/user/sy-waybar.service");
    assert!(unit.contains(
        "ExecStartPost=/usr/bin/systemctl --user --no-block try-restart sy-stack-bar.service"
    ));
}

#[test]
fn stack_waits_for_waybars_surface_instead_of_a_startup_delay() {
    let unit = include_str!("../configs/systemd/user/sy-stack-bar.service");
    assert!(unit.contains("niri msg --json layers"));
    assert!(!unit.contains("pgrep"));
}

#[test]
fn panel_apply_preserves_optional_dialpad_window_rules() {
    let config = include_str!("../configs/niri/config.kdl");
    assert!(config.contains("include optional=true \"../asus-dialpad-driver/niri-dialpad.kdl\""));
}
