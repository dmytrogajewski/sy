use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn fake(dir: &std::path::Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn base_help_works_without_any_feature() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .arg("--help")
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Optional packages"));
}

#[test]
fn installed_desktop_supplies_full_help() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "sy-all", "printf 'full desktop help\\n'");
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .arg("--help")
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "full desktop help\n"
    );
}

#[test]
fn installed_desktop_help_uses_public_command_name() {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), dir.path().join("sy-all"))
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .arg("--help")
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("Usage: sy [OPTIONS]"));
}

#[test]
fn power_runs_without_the_desktop_binary() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "sy-system-power", "printf '%s\\n' \"$@\"");
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .args(["power", "reboot", "--dry-run"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "reboot\n--dry-run\n"
    );
}

#[test]
fn agent_runs_without_the_desktop_binary() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "sy-system-agent", "printf '%s\\n' \"$@\"");
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .args(["agt", "list", "--json"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "agt\nlist\n--json\n"
    );
}

#[test]
fn aiplane_runs_without_the_desktop_binary() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "sy-system-aiplane", "printf '%s\\n' \"$@\"");
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .args(["aiplane", "list", "--json"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "aiplane\nlist\n--json\n"
    );
}

#[test]
fn auth_runs_without_the_desktop_binary() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "sy-system-auth", "printf '%s\\n' \"$@\"");
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .args(["syauth", "doctor"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "syauth\ndoctor\n"
    );
}

#[test]
fn missing_auth_names_its_package() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .args(["syauth", "doctor"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(69));
    assert!(String::from_utf8_lossy(&output.stderr).contains("sy-system-auth"));
}

#[test]
fn missing_aiplane_names_its_package() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .arg("aiplane")
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(69));
    assert!(String::from_utf8_lossy(&output.stderr).contains("sy-system-aiplane"));
}

#[test]
fn filemanager_runs_without_the_desktop_binary() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "sy-filemanager", "printf '%s\\n' \"$@\"");
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .args(["file", "doctor", "--json"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "file\ndoctor\n--json\n"
    );
}

#[test]
fn knowledge_command_preserves_arguments_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    fake(
        dir.path(),
        "sy-system-knowledge",
        "printf '%s\\n' \"$@\"; exit 17",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .args(["--root", "/usr/share/sy", "knowledge", "status", "--json"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "--root\n/usr/share/sy\nknowledge\nstatus\n--json\n"
    );
}

#[test]
fn auto_uses_the_knowledge_package() {
    let dir = tempfile::tempdir().unwrap();
    fake(
        dir.path(),
        "sy-system-knowledge",
        "printf 'knowledge feature\\n'",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .args(["auto", "--help"])
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "knowledge feature\n"
    );
}

#[test]
fn monitor_and_shell_route_to_independent_features() {
    let dir = tempfile::tempdir().unwrap();
    fake(dir.path(), "sy-system-monitor", "printf 'monitor\\n'");
    fake(dir.path(), "sy-desktop-shell", "printf 'shell\\n'");
    for (command, expected) in [
        ("mon", "monitor\n"),
        ("stack", "shell\n"),
        ("wifi", "shell\n"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
            .arg(command)
            .env("SY_FEATURE_BIN_DIR", dir.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
}

#[test]
fn missing_feature_identifies_the_package() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy-launcher"))
        .arg("power")
        .env("SY_FEATURE_BIN_DIR", dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(69));
    assert!(String::from_utf8_lossy(&output.stderr).contains("sy-system-power"));
}
