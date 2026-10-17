use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn power_delegates_to_its_optional_executable() {
    let dir = tempfile::tempdir().unwrap();
    let feature = dir.path().join("sy-system-power");
    std::fs::write(&feature, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    std::fs::set_permissions(&feature, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["power", "reboot", "--dry-run", "--json"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .expect("run sy power");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "reboot\n--dry-run\n--json\n"
    );
}

#[test]
fn missing_power_package_has_install_hint() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["power", "reboot", "--dry-run"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("sy-system-power package"));
}

#[test]
fn locally_installed_power_works_without_an_rpm() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".local/bin");
    std::fs::create_dir_all(&dir).unwrap();
    let feature = dir.join("sy-system-power");
    std::fs::write(&feature, "#!/bin/sh\nprintf 'local-power\\n'\n").unwrap();
    std::fs::set_permissions(&feature, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["power", "reboot", "--dry-run"])
        .env("HOME", home.path())
        .env_remove("SY_FEATURE_BIN_DIR")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "local-power\n");
}

#[test]
fn desktop_has_menu_entry_points() {
    let niri = include_str!("../configs/niri/config.kdl");
    let waybar = include_str!("../configs/waybar/config.jsonc");
    assert!(niri.contains("Mod+Shift+Escape"));
    assert!(niri.contains("\"power\""));
    assert!(waybar.contains("custom/sy-power"));
    assert!(waybar.contains("{{ sy_bin }} power"));
}
