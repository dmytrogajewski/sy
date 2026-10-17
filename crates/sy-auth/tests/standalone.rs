use std::process::Command;

#[test]
fn auth_help_works_without_desktop() {
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-auth"))
        .args(["syauth", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("install-pam"));
}

#[test]
fn auth_accepts_launcher_global_paths() {
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-auth"))
        .args([
            "--root",
            "/usr/share/sy",
            "--target",
            "/tmp/sy",
            "syauth",
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
