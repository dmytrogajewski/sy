use std::process::Command;

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
