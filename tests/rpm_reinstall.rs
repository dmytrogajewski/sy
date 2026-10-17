use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn reinstall_dry_run_preserves_a_base_only_install() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    let rpms = temp.path().join("rpms/f43");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&rpms).unwrap();
    for name in [
        "sy",
        "sy-desktop",
        "sy-desktop-shell",
        "sy-system-power",
        "sy-system-agent",
        "sy-system-auth",
        "sy-system-aiplane",
        "sy-filemanager",
    ] {
        fs::write(rpms.join(format!("{name}-0.1.0.rpm")), "").unwrap();
    }
    let rpm = bin.join("rpm");
    fs::write(&rpm, "#!/bin/sh\ncase \"$1\" in\n-E) echo 43;;\n-qp) basename \"$4\" | sed 's/-0.1.0.rpm//';;\n-q) test \"$2\" = sy;;\nesac\n").unwrap();
    fs::set_permissions(&rpm, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("bash")
        .args(["scripts/reinstall-rpms.sh", "--dry-run"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("SY_RPM_OUTPUT_DIR", temp.path().join("rpms"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sy-desktop-"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sy-desktop-shell-"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sy-system-auth-"));
    let full = Command::new("bash")
        .args(["scripts/reinstall-rpms.sh", "--dry-run", "--all"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("SY_RPM_OUTPUT_DIR", temp.path().join("rpms"))
        .output()
        .unwrap();
    assert!(
        full.status.success(),
        "{}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert!(String::from_utf8_lossy(&full.stdout).contains("sy-system-auth-"));
}

#[test]
fn reinstall_preserves_an_existing_desktop_feature_set() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    let rpms = temp.path().join("rpms/f43");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&rpms).unwrap();
    for name in [
        "sy",
        "sy-desktop",
        "sy-desktop-shell",
        "sy-system-power",
        "sy-system-aiplane",
        "sy-system-knowledge",
    ] {
        fs::write(rpms.join(format!("{name}-0.1.0.rpm")), "").unwrap();
    }
    let rpm = bin.join("rpm");
    fs::write(&rpm, "#!/bin/sh\ncase \"$1\" in\n-E) echo 43;;\n-qp) basename \"$4\" | sed 's/-0.1.0.rpm//';;\n-q) case \"$2\" in sy|sy-desktop|sy-system-power) exit 0;; *) exit 1;; esac;;\nesac\n").unwrap();
    fs::set_permissions(&rpm, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("bash")
        .args(["scripts/reinstall-rpms.sh", "--dry-run"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("SY_RPM_OUTPUT_DIR", temp.path().join("rpms"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("sy-desktop-"), "{stdout}");
    assert!(stdout.contains("sy-desktop-shell-"), "{stdout}");
    assert!(stdout.contains("sy-system-power-"), "{stdout}");
    assert!(!stdout.contains("sy-system-aiplane-"), "{stdout}");
    assert!(!stdout.contains("sy-system-knowledge-"), "{stdout}");
}
