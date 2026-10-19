use std::process::Command;

#[test]
fn failed_clipboard_decode_does_not_launch_preview_or_create_export() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let decoder = dir.path().join("cliphist");
    std::fs::write(&decoder, "#!/bin/sh\nprintf stale\nexit 1\n").unwrap();
    std::fs::set_permissions(&decoder, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy-desktop-shell"))
        .args(["stack", "action", "55", "preview", "--source", "clip"])
        .env("PATH", dir.path())
        .env("XDG_RUNTIME_DIR", dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("cliphist decode 55 failed"));
    assert!(!dir.path().join("sy/clip").exists());
}

#[test]
fn standalone_shell_exposes_stack_and_applet_help() {
    let exe = env!("CARGO_BIN_EXE_sy-desktop-shell");
    for command in ["stack", "vol", "wifi", "profile"] {
        let output = Command::new(exe)
            .args([command, "--help"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn standalone_shell_lists_empty_stack_as_json() {
    let state = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy-desktop-shell"))
        .args(["stack", "list", "--json"])
        .env("XDG_STATE_HOME", state.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["items"], serde_json::json!([]));
}

#[test]
fn shell_accepts_launcher_global_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_sy-desktop-shell"))
        .args([
            "--root",
            "/usr/share/sy",
            "--target",
            "/tmp",
            "stack",
            "--help",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
