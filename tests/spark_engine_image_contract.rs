use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

fn image_label(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.trim().strip_prefix("sy.spark.")?.split_once("=\"")?;
    Some((key, value.trim_end_matches('\\').trim().trim_matches('"')))
}

fn image_label_named<'a>(source: &'a str, expected: &str) -> Option<&'a str> {
    source.lines().find_map(|line| {
        image_label(line).and_then(|(key, value)| (key == expected).then_some(value))
    })
}

fn policy_token(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn copied_destination<'a>(source: &'a str, asset: &str) -> Option<&'a str> {
    source.lines().find_map(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        (fields.len() == 3 && fields[0] == "COPY" && fields[1].rsplit('/').next() == Some(asset))
            .then(|| fields[2])
    })
}

#[test]
fn image_patch_assets_are_content_addressed() {
    let directory =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines/patches");
    let entries: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .collect();
    assert!(!entries.is_empty());
    for entry in entries {
        let name = entry.file_name().into_string().unwrap();
        let expected = name
            .strip_prefix("sha256-")
            .unwrap()
            .split('.')
            .next()
            .unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(fs::read(entry.path()).unwrap())),
            expected
        );
    }
}

#[test]
fn contracted_images_apply_existing_patch_assets() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(&directory).unwrap().filter_map(Result::ok) {
        if entry
            .path()
            .extension()
            .is_none_or(|ext| ext != "Dockerfile")
        {
            continue;
        }
        let dockerfile = fs::read_to_string(entry.path()).unwrap();
        if !dockerfile.contains("sy.spark.image-contract=\"v1\"") {
            continue;
        }
        let assets: Vec<_> = dockerfile
            .lines()
            .filter_map(|line| line.strip_prefix("COPY patches/"))
            .collect();
        assert!(!assets.is_empty());
        assert!(assets.iter().all(|line| {
            directory
                .join("patches")
                .join(line.split_whitespace().next().unwrap())
                .is_file()
        }));
        assert!(dockerfile.contains("sha256sum --check"));
    }
}

#[test]
fn every_patch_asset_is_referenced() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    let dockerfiles = fs::read_dir(&directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .collect::<Vec<_>>();
    let referenced = dockerfiles
        .iter()
        .flat_map(|file| file.lines())
        .filter_map(|line| line.strip_prefix("COPY patches/"))
        .filter_map(|line| line.split_whitespace().next())
        .collect::<HashSet<_>>();
    for entry in fs::read_dir(directory.join("patches"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
    {
        assert!(referenced.contains(entry.file_name().to_str().unwrap()));
    }
}

#[test]
fn configured_image_self_tests_are_executed() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    let dockerfiles = fs::read_dir(directory).unwrap().filter_map(Result::ok);
    let mut discovered = 0;
    for entry in dockerfiles {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if !source.contains("sy.spark.image-contract=\"v1\"") {
            continue;
        }
        let profile: toml::Value =
            toml::from_str(&fs::read_to_string(entry.path().with_extension("toml")).unwrap())
                .unwrap();
        let command = profile["entrypoint"][0].as_str().unwrap();
        for (_, asset) in source
            .lines()
            .filter_map(image_label)
            .filter(|(key, _)| key.ends_with("-self-test"))
        {
            let destination = copied_destination(&source, asset).unwrap();
            assert!(source.contains(&format!("{command} {destination}")));
            discovered += 1;
        }
    }
    assert!(discovered > 0);
}

#[test]
fn configured_persistence_assets_are_copied_and_executed() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    let mut discovered = 0;
    for entry in fs::read_dir(&directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if !source.contains("sy.spark.image-contract=\"v1\"") {
            continue;
        }
        let profile: toml::Value =
            toml::from_str(&fs::read_to_string(entry.path().with_extension("toml")).unwrap())
                .unwrap();
        let command = profile["entrypoint"][0].as_str().unwrap();
        for key in ["persistence-transformer", "persistence-self-test"] {
            let Some(asset) = source.lines().find_map(|line| {
                line.trim()
                    .strip_prefix(&format!("sy.spark.{key}="))
                    .map(|value| value.trim_end_matches('\\').trim().trim_matches('"'))
            }) else {
                continue;
            };
            let copy = source
                .lines()
                .find(|line| line.starts_with(&format!("COPY patches/{asset} ")))
                .unwrap();
            assert!(source.contains(&format!(
                "{command} {}",
                copy.split_whitespace().last().unwrap()
            )));
            discovered += 1;
        }
    }
    assert!(discovered > 0);
}

#[test]
fn persistent_mmap_images_reclaim_validation_page_cache() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(&directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.persistence-transformer=") {
            for key in ["page-cache-transformer", "page-cache-self-test"] {
                let asset = image_label_named(&source, key).unwrap();
                let destination = copied_destination(&source, asset).unwrap();
                assert!(source.contains(&format!("python3 {destination}")));
            }
        }
    }
}

#[test]
fn warmed_persistent_images_flush_transient_allocator_state() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(&directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        let profile = fs::read_to_string(entry.path().with_extension("toml")).unwrap_or_default();
        if source.contains("sy.spark.persistence-transformer=") && profile.contains("--warmups") {
            let policy: toml::Value = toml::from_str(&profile).unwrap();
            let cleanup = image_label_named(&source, "post-warmup-cleanup").unwrap();
            let arguments = policy["profiles"][0]["arguments"].as_array().unwrap();
            let warmups = arguments
                .windows(2)
                .find(|pair| pair[0].as_str() == Some("--warmups"))
                .unwrap()[1]
                .as_str()
                .unwrap()
                .split(',')
                .collect::<Vec<_>>();
            assert!(warmups.len() > 1 && warmups.last() == Some(&cleanup));
            for key in ["post-warmup-transformer", "post-warmup-self-test"] {
                let asset = image_label_named(&source, key).unwrap();
                assert!(source.contains(&format!(
                    "python3 {}",
                    copied_destination(&source, asset).unwrap()
                )));
            }
        }
    }
}

#[test]
fn contracted_images_self_test_their_matching_profile() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            let profile = entry.path().with_extension("toml");
            assert!(profile.is_file());
            let name = profile.file_name().unwrap().to_string_lossy();
            let destination = copied_destination(&source, &name).unwrap();
            assert_eq!(
                destination,
                image_label_named(&source, "runtime-profile").unwrap()
            );
        }
    }
}

#[test]
fn contracted_images_attest_the_configured_nonroot_user() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            let profile = fs::read_to_string(entry.path().with_extension("toml")).unwrap();
            let policy: toml::Value = toml::from_str(&profile).unwrap();
            let uid = policy["run_as_uid"].as_integer().unwrap();
            assert!(uid > 0);
            assert!(source.contains(&format!("sy.spark.runtime-user=\"{uid}\"")));
            assert!(source.contains(&format!("USER {uid}:{uid}")));
        }
    }
}

#[test]
fn contracted_runtime_self_tests_parse_the_copied_profile() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            let policy: toml::Value =
                toml::from_str(&fs::read_to_string(entry.path().with_extension("toml")).unwrap())
                    .unwrap();
            let command = policy["entrypoint"][0].as_str().unwrap();
            let asset = image_label_named(&source, "runtime-self-test").unwrap();
            let destination = copied_destination(&source, asset).unwrap();
            let profile = image_label_named(&source, "runtime-profile").unwrap();
            assert!(source.contains(&format!("{command} {destination} {profile}")));
        }
    }
}

#[test]
fn configured_image_self_tests_run_as_the_declared_user() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            let profile: toml::Value =
                toml::from_str(&fs::read_to_string(entry.path().with_extension("toml")).unwrap())
                    .unwrap();
            let user = format!("USER {0}:{0}", profile["run_as_uid"].as_integer().unwrap());
            let command = profile["entrypoint"][0].as_str().unwrap();
            let boundary = source.find(&user).unwrap();
            for (_, asset) in source
                .lines()
                .filter_map(image_label)
                .filter(|(key, _)| key.ends_with("-self-test"))
            {
                let destination = copied_destination(&source, asset).unwrap();
                assert!(source.find(&format!("{command} {destination}")).unwrap() > boundary);
            }
        }
    }
}

#[test]
fn non_root_image_self_tests_receive_the_declared_home() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            let policy: toml::Value =
                toml::from_str(&fs::read_to_string(entry.path().with_extension("toml")).unwrap())
                    .unwrap();
            let home = policy["environment"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(toml::Value::as_str)
                .find(|assignment| assignment.starts_with("HOME="))
                .unwrap();
            let user = format!("USER {0}:{0}", policy["run_as_uid"].as_integer().unwrap());
            assert!(source.find(&format!("ENV {home}")).unwrap() < source.find(&user).unwrap());
        }
    }
}

#[test]
fn contracted_profiles_attest_offline_private_networking() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            let profile: toml::Value =
                toml::from_str(&fs::read_to_string(entry.path().with_extension("toml")).unwrap())
                    .unwrap();
            let network = profile["network"].as_str().unwrap();
            assert!(!matches!(network, "bridge" | "host" | "none"));
            let environment = profile["environment"].as_array().unwrap();
            assert!(environment
                .iter()
                .filter_map(toml::Value::as_str)
                .any(|value| value.ends_with("_OFFLINE=1")));
            assert!(policy_token(
                image_label_named(&source, "runtime-network").unwrap()
            ));
        }
    }
}

#[test]
fn contracted_profiles_limit_writes_to_declared_mounts() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            let profile: toml::Value =
                toml::from_str(&fs::read_to_string(entry.path().with_extension("toml")).unwrap())
                    .unwrap();
            let writable = image_label_named(&source, "runtime-writable")
                .unwrap()
                .split(',')
                .collect::<Vec<_>>();
            let tmpfs = profile["tmpfs"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>();
            assert!(tmpfs.iter().all(|path| writable.contains(path)));
            let environment = profile["environment"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(toml::Value::as_str)
                .collect::<Vec<_>>();
            let mut executable_paths = Vec::new();
            for name in profile["executable_cache_environment"].as_array().unwrap() {
                let name = name.as_str().unwrap();
                let value = environment
                    .iter()
                    .find_map(|assignment| assignment.strip_prefix(&format!("{name}=")))
                    .unwrap();
                assert!(writable
                    .iter()
                    .any(|root| Path::new(value).starts_with(root)));
                executable_paths.push(value);
            }
            assert!(writable.iter().all(|root| tmpfs.contains(root)
                || executable_paths
                    .iter()
                    .any(|path| Path::new(path).starts_with(root))));
        }
    }
}

#[test]
fn non_tmpfs_writable_roots_exist_before_non_root_self_tests() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            let policy: toml::Value =
                toml::from_str(&fs::read_to_string(entry.path().with_extension("toml")).unwrap())
                    .unwrap();
            let uid = policy["run_as_uid"].as_integer().unwrap();
            let tmpfs = policy["tmpfs"].as_array().unwrap();
            let boundary = source.find(&format!("USER {uid}:{uid}")).unwrap();
            for root in image_label_named(&source, "runtime-writable")
                .unwrap()
                .split(',')
                .filter(|root| !tmpfs.iter().any(|path| path.as_str() == Some(root)))
            {
                let provision = format!("install -d -o {uid} -g {uid} -m 0700 {root}");
                assert!(source.find(&provision).unwrap() < boundary);
            }
        }
    }
}

#[test]
fn contracted_runtimes_attest_their_jit_tool_policy() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            assert!(policy_token(
                image_label_named(&source, "runtime-build-tools").unwrap()
            ));
            let mut declared = Vec::new();
            for (label, command) in [
                ("runtime-jit-tools", "command -v \"${tool}\""),
                (
                    "runtime-rejected-tools",
                    "if command -v \"${tool}\"; then exit 1; fi",
                ),
            ] {
                let tools = image_label_named(&source, label).unwrap().replace(',', " ");
                assert!(!tools.is_empty());
                assert!(source.contains(&format!("for tool in {tools}; do")));
                assert!(source.contains(command));
                declared.push(
                    tools
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect::<HashSet<_>>(),
                );
            }
            assert!(declared[0].is_disjoint(&declared[1]));
            assert!(policy_token(
                image_label_named(&source, "runtime-source-policy").unwrap()
            ));
            let checkout = image_label_named(&source, "runtime-source").unwrap();
            let metadata = image_label_named(&source, "runtime-scm-metadata").unwrap();
            assert!(source.contains(&format!("chmod -R a-w {checkout}")));
            assert!(source.contains(&format!("test ! -e {metadata}")));
        }
    }
}

#[test]
fn operational_image_metadata_is_not_duplicated_in_rust_contracts() {
    let rust = include_str!("spark_engine_image_contract.rs");
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    for entry in fs::read_dir(directory).unwrap().filter_map(Result::ok) {
        let source = fs::read_to_string(entry.path()).unwrap_or_default();
        if source.contains("sy.spark.image-contract=\"v1\"") {
            for (key, value) in source.lines().filter_map(image_label) {
                if key.starts_with("runtime-") && !value.starts_with('/') && !value.contains(',') {
                    assert!(
                        !rust.contains(value),
                        "runtime policy value duplicated in Rust"
                    );
                }
                for path in value.split(',').filter(|value| value.starts_with('/')) {
                    assert!(
                        !rust.contains(path),
                        "image path duplicated in Rust: {path}"
                    );
                }
            }
        }
    }
}
