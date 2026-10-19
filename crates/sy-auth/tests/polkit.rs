use std::{fs, path::Path, process::Command};

#[test]
fn polkit_uses_sudo_authentication_and_keeps_its_own_account_stack() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let config = fs::read_to_string(repo.join("configs/pam/polkit-1")).unwrap();
    assert!(
        config
            .lines()
            .any(|line| line == "auth       include      sudo")
    );
    assert!(config.contains("account    include      system-auth"));
    assert!(!config.contains("pam_u2f.so"));
}

#[test]
fn installer_dry_run_does_not_write_to_the_pam_directory() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = tempfile::tempdir().unwrap();
    let output = Command::new("bash")
        .arg(repo.join("scripts/install-system-auth.sh"))
        .args(["--root", root.path().to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("etc").exists());
}

#[test]
fn installer_refuses_a_missing_sudo_authentication_stack() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = tempfile::tempdir().unwrap();
    let output = Command::new("bash")
        .arg(repo.join("scripts/install-system-auth.sh"))
        .args(["--root", root.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!root.path().join("etc/pam.d/polkit-1").exists());
}

#[test]
fn installer_preserves_existing_override_and_is_repeatable() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = tempfile::tempdir().unwrap();
    let pam = root.path().join("etc/pam.d");
    fs::create_dir_all(&pam).unwrap();
    fs::write(pam.join("sudo"), "auth include system-auth\n").unwrap();
    fs::write(pam.join("polkit-1"), "original\n").unwrap();
    for _ in 0..2 {
        assert!(
            Command::new("bash")
                .arg(repo.join("scripts/install-system-auth.sh"))
                .args(["--root", root.path().to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
    }
    assert_eq!(
        fs::read_to_string(pam.join("polkit-1.sy-backup")).unwrap(),
        "original\n"
    );
    assert_eq!(
        fs::read(pam.join("polkit-1")).unwrap(),
        fs::read(repo.join("configs/pam/polkit-1")).unwrap()
    );
}

#[test]
fn session_starts_the_supervised_sy_agent() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let unit =
        fs::read_to_string(repo.join("configs/systemd/user/sy-polkit-agent.service")).unwrap();
    assert!(unit.contains("ExecStart=%h/.local/libexec/sy/sy-desktop-shell polkit-agent"));
    assert!(unit.contains("WantedBy=graphical-session.target"));
    let niri = fs::read_to_string(repo.join("configs/niri/config.kdl")).unwrap();
    assert!(!niri.contains("spawn-at-startup \"lxpolkit\""));
    assert!(niri.contains("sy-polkit-agent.service"));
}

#[test]
fn agent_installer_supports_a_dry_run_without_creating_files() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let home = tempfile::tempdir().unwrap();
    let output = Command::new("bash")
        .arg(repo.join("scripts/install-polkit-agent.sh"))
        .env("SY_POLKIT_HOME", home.path())
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn agent_installer_copies_binary_and_migrates_startup_repeatably() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let home = tempfile::tempdir().unwrap();
    let niri = home.path().join(".config/niri/config.kdl");
    fs::create_dir_all(niri.parent().unwrap()).unwrap();
    fs::write(&niri, "spawn-at-startup \"lxpolkit\"\n").unwrap();
    for _ in 0..2 {
        assert!(
            Command::new("bash")
                .arg(repo.join("scripts/install-polkit-agent.sh"))
                .env("SY_POLKIT_HOME", home.path())
                .args(["--binary", "/usr/bin/true"])
                .status()
                .unwrap()
                .success()
        );
    }
    assert_eq!(
        fs::read(home.path().join(".local/libexec/sy/sy-desktop-shell")).unwrap(),
        fs::read("/usr/bin/true").unwrap()
    );
    assert!(
        fs::read_to_string(&niri)
            .unwrap()
            .contains("sy-polkit-agent.service")
    );
    assert_eq!(
        fs::read_to_string(niri.with_extension("kdl.sy-polkit-backup")).unwrap(),
        "spawn-at-startup \"lxpolkit\"\n"
    );
}
