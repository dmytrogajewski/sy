use std::{fs, path::Path};

#[test]
fn spark_release_inventory_and_policy_are_repository_owned() {
    let workflow = fs::read_to_string(".github/workflows/spark-release.yml").unwrap();
    let policy = fs::read_to_string("deny.toml").unwrap();
    for required in [
        "cargo deny --no-default-features --features spark-agent check",
        "cargo auditable zigbuild",
        "aarch64-unknown-linux-gnu",
        "resolved-features.txt",
        "duplicate-dependencies.txt",
        "native-unsafe-build-inventory.txt",
        "SHA256SUMS",
        "minisign -Sm",
    ] {
        assert!(
            workflow.contains(required),
            "missing release gate: {required}"
        );
    }
    for required in ["[advisories]", "[licenses]", "[bans]", "[sources]"] {
        assert!(
            policy.contains(required),
            "missing dependency policy: {required}"
        );
    }
    assert!(policy.contains("expires 2026-09-15"));
    assert!(policy.contains("RUSTSEC-2026-0258"));
}

#[test]
fn spark_supervision_and_lsm_assets_keep_the_split_boundary() {
    let target = fs::read_to_string("configs/systemd/system/sy-spark.target").unwrap();
    let agent = fs::read_to_string("configs/systemd/system/sy-spark-agent.service").unwrap();
    let executor = fs::read_to_string("configs/systemd/system/sy-spark-executor.service").unwrap();
    let agent_lsm = fs::read_to_string("configs/apparmor.d/sy-spark-agent").unwrap();
    let executor_lsm = fs::read_to_string("configs/apparmor.d/sy-spark-executor").unwrap();
    assert!(target.contains("Requires=sy-spark-executor.service sy-spark-agent.service"));
    assert!(agent.contains("PartOf=sy-spark.target"));
    assert!(executor.contains("PrivateNetwork=yes"));
    assert!(agent_lsm.contains("deny /var/run/docker.sock rw,"));
    assert!(executor_lsm.contains("deny network inet,"));
    assert!(Path::new("specs/openapi/sy-spark-control-v1.json").is_file());
}

#[test]
fn optional_disruptive_maintenance_is_never_encoded_as_automatic_work() {
    let installer = fs::read_to_string("src/spark/install.rs").unwrap();
    assert!(installer.contains("docker_restart: \"not_run\".into()"));
    assert!(installer.contains("host_reboot: \"not_run\".into()"));
    for forbidden in ["restart docker", "systemctl reboot", "shutdown -r"] {
        assert!(!installer.to_ascii_lowercase().contains(forbidden));
    }
}
