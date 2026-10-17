use std::process::Command;

#[test]
fn standalone_knowledge_cli_exposes_daemon_and_json_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-knowledge"))
        .args(["knowledge", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("daemon"), "{help}");
    assert!(help.contains("status"), "{help}");
}

#[test]
fn standalone_knowledge_writes_a_user_config_without_desktop_sources() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("notes");
    std::fs::create_dir(&source).unwrap();
    let config = temp.path().join("config/sy/sy.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-knowledge"))
        .args(["knowledge", "add"])
        .arg(&source)
        .env("SY_KNOWLEDGE_CONFIG_PATH", &config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = std::fs::read_to_string(config).unwrap();
    assert!(body.contains("[[knowledge.sources]]"), "{body}");
}
