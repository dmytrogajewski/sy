use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::Command,
};

use serde_json::json;
use sha2::Digest;

fn fixture(root: &std::path::Path, corrupt: bool) -> std::path::PathBuf {
    let model = root.join("model");
    let source = root.join("source");
    let destination = root.join("destination");
    fs::create_dir_all(&model).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o750)).unwrap();
    let tensors = [
        ("table.shard_0.weight", b"abcd"),
        ("table.shard_1.weight", b"efgh"),
    ];
    let mut offsets = 0_u64;
    let mut header = serde_json::Map::new();
    for (name, bytes) in tensors {
        header.insert(
            name.into(),
            json!({"dtype":"U8","shape":[bytes.len()],"data_offsets":[offsets, offsets + bytes.len() as u64]}),
        );
        offsets += bytes.len() as u64;
    }
    let mut encoded = serde_json::to_vec(&header).unwrap();
    while !encoded.len().is_multiple_of(8) {
        encoded.push(b' ');
    }
    let mut shard = (encoded.len() as u64).to_le_bytes().to_vec();
    shard.extend(encoded);
    shard.extend(tensors.into_iter().flat_map(|(_, bytes)| bytes));
    fs::write(model.join("weights.safetensors"), shard).unwrap();
    fs::create_dir(model.join("blobs")).unwrap();
    fs::write(
        model.join("blobs/index.json"),
        serde_json::to_vec(&json!({"weight_map": {
            "table.shard_0.weight": "weights.safetensors",
            "table.shard_1.weight": "weights.safetensors"
        }}))
        .unwrap(),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        "blobs/index.json",
        model.join("model.safetensors.index.json"),
    )
    .unwrap();
    let artifact = source.join("table.bin");
    fs::write(&artifact, if corrupt { b"abcdxxxx" } else { b"abcdefgh" }).unwrap();
    fs::write(
        source.join("table.bin.complete.json"),
        serde_json::to_vec(&json!({
            "schema":"cache/v1", "transform":"legacy", "shape":[8], "dtype":"u8", "bytes":8
        }))
        .unwrap(),
    )
    .unwrap();
    let digest = format!("{:x}", sha2::Sha256::digest(b"abcdefgh"));
    let identity = "/models/snapshots/immutable";
    let admission = root.join("admission.json");
    fs::write(
        &admission,
        serde_json::to_vec(&json!({
            "schema":"sy.spark.admission-report/v1",
            "selection":{"compile_cache_namespace":"identity/instance"}
        }))
        .unwrap(),
    )
    .unwrap();
    let contract = root.join("contract.json");
    fs::write(
        &contract,
        serde_json::to_vec(&json!({
            "schema":"sy.spark.cache-promotion/v1",
            "model_root": model,
            "model_index":"model.safetensors.index.json",
            "tensor_pattern":"^table\\.shard_([0-9]+)\\.weight$",
            "content_transform":"ordered-safetensors-tensor-concatenation/v1",
            "model_identity":identity,
            "source_artifact":artifact,
            "source_marker":source.join("table.bin.complete.json"),
            "destination_root":destination,
            "admission_report":admission,
            "destination_subdirectory":"ple",
            "legacy_marker":{
                "schema":"cache/v1", "transform":"legacy", "shape":[8], "dtype":"u8", "bytes":8
            },
            "target_marker":{
                "schema":"cache/v2", "transform":"verified", "source":format!("sha256:{:x}", sha2::Sha256::digest(identity)),
                "shape":[8], "dtype":"u8", "bytes":8, "sha256":digest
            }
        }))
        .unwrap(),
    )
    .unwrap();
    contract
}

#[test]
fn exact_source_validation_promotes_a_cache_inode_without_copying_it() {
    let root = tempfile::tempdir().unwrap();
    let contract = fixture(root.path(), false);
    fs::create_dir(root.path().join("destination/identity")).unwrap();
    let trace = root.path().join("fadvise.trace");
    let output = Command::new("strace")
        .args(["-e", "trace=fadvise64", "-o"])
        .arg(&trace)
        .arg("python3")
        .arg("scripts/promote-spark-cache.py")
        .arg(&contract)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let source = fs::metadata(root.path().join("source/table.bin")).unwrap();
    let namespace = fs::metadata(root.path().join("destination/identity/instance")).unwrap();
    let promoted = fs::metadata(
        root.path()
            .join("destination/identity/instance/ple/table.bin"),
    )
    .unwrap();
    let directory = fs::metadata(root.path().join("destination/identity/instance/ple")).unwrap();
    assert_eq!(
        (source.dev(), source.ino()),
        (promoted.dev(), promoted.ino())
    );
    assert_eq!(promoted.permissions().mode() & 0o777, 0o440);
    assert_eq!(promoted.nlink(), 2);
    assert_eq!(namespace.permissions().mode() & 0o777, 0o770);
    assert_eq!(
        (namespace.uid(), namespace.gid()),
        (directory.uid(), directory.gid())
    );
    assert_eq!(directory.permissions().mode() & 0o777, 0o770);
    assert!(fs::read_to_string(trace)
        .unwrap()
        .contains("POSIX_FADV_DONTNEED"));
}

#[test]
fn content_mismatch_is_rejected_before_destination_publication() {
    let root = tempfile::tempdir().unwrap();
    let contract = fixture(root.path(), true);
    let status = Command::new("python3")
        .arg("scripts/promote-spark-cache.py")
        .arg(&contract)
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(!root.path().join("destination/identity").exists());
}
