use std::process::Command;

#[test]
fn policy_lint_works_outside_the_source_tree_with_root_override() {
    let cwd = tempfile::tempdir().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-agent"))
        .args(["--root", root.to_str().unwrap(), "policy", "lint", "--json"])
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
