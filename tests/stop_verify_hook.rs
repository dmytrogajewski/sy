use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
};

fn collect_hook_output(mut child: Child, input: &[u8]) -> String {
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn run_hook_process(project: &Path) -> String {
    let hook = Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude/hooks/stop-verify.sh");
    let child = Command::new("bash")
        .arg(hook)
        .env("CLAUDE_PROJECT_DIR", project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    collect_hook_output(child, b"{\"stop_hook_active\":false}\n")
}

fn run_stop_hook(source: &str) -> String {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='hook-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(project.path().join("src/lib.rs"), source).unwrap();
    run_hook_process(project.path())
}

fn run_post_edit_hook(source: &str) -> String {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join("fixture.rs");
    fs::write(&path, source).unwrap();
    run_post_edit_process(&path)
}

fn run_post_edit_process(path: &Path) -> String {
    let hook = Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude/hooks/post-edit-check.sh");
    let child = Command::new("bash")
        .arg(hook)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = format!(r#"{{"tool_input":{{"file_path":"{}"}}}}"#, path.display());
    collect_hook_output(child, input.as_bytes())
}

#[test]
fn legitimate_validation_identifier_does_not_block_stop() {
    let source = ["pub fn validate_place", "holders() {}\n"].concat();
    let output = run_stop_hook(&source);

    assert!(output.is_empty(), "unexpected stop-hook block: {output}");
}

#[test]
fn explicit_deferred_work_marker_blocks_stop() {
    let source = ["pub fn unfinished() { // TO", "DO\n}\n"].concat();
    let output = run_stop_hook(&source);

    assert!(output.contains("\"decision\": \"block\""));
}

#[test]
fn legitimate_validation_identifier_does_not_trigger_post_edit() {
    let source = ["pub fn validate_place", "holders() {}\n"].concat();
    let output = run_post_edit_hook(&source);

    assert!(output.is_empty(), "unexpected post-edit warning: {output}");
}

#[test]
fn explicit_deferred_work_marker_triggers_post_edit() {
    let source = ["pub fn unfinished() { // TO", "DO\n}\n"].concat();
    let output = run_post_edit_hook(&source);

    assert!(output.contains("placeholder-shaped code detected"));
}
