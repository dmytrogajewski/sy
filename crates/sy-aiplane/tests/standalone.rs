use std::process::Command;

#[test]
fn list_works_without_the_desktop_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-aiplane"))
        .args(["aiplane", "list", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("embed"));
}
