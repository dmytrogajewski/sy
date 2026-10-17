use std::process::Command;

#[test]
fn file_cli_works_without_the_desktop_bundle() {
    let output = Command::new(env!("CARGO_BIN_EXE_sy-filemanager"))
        .args(["file", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("doctor"));
}
