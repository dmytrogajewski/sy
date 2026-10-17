use std::process::Command;

#[test]
fn power_can_run_without_the_main_sy_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-power"))
        .args(["reboot", "--dry-run", "--json"])
        .output()
        .expect("run standalone power feature");
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], serde_json::json!(["systemctl", "reboot"]));
}
