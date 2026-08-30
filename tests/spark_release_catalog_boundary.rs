#[test]
fn production_installer_does_not_embed_operational_catalogs() {
    let source = include_str!("../src/spark/install.rs");
    let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
    let compact: String = production.chars().filter(|c| !c.is_whitespace()).collect();
    let package = include_str!("../scripts/package-spark-release.sh");
    assert!(!compact.contains("include_bytes!(\"../../configs/sy/spark/engines/"));
    assert!(!compact.contains("include_str!(\"../../configs/sy/spark/engines/"));
    for name in engine_config_names() {
        assert!(!production.contains(&name));
        assert!(!package.contains(&name));
    }
    for name in model_catalog_names() {
        assert!(!compact.contains(&format!(
            "include_bytes!(\"../../configs/sy/spark/{name}\")"
        )));
        assert!(!compact.contains(&format!("include_str!(\"../../configs/sy/spark/{name}\")")));
    }
}

#[test]
fn engine_image_inputs_are_immutable() {
    let dockerfiles = engine_dockerfiles();
    assert!(!dockerfiles.is_empty());
    for dockerfile in dockerfiles {
        let image = dockerfile
            .lines()
            .find_map(|line| line.strip_prefix("FROM "))
            .unwrap();
        let digest = image.split_once("@sha256:").unwrap().1;
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!dockerfile.contains(":latest"));
    }
}

#[test]
fn engine_images_record_frozen_provenance() {
    for dockerfile in engine_dockerfiles() {
        assert!(dockerfile.contains("org.opencontainers.image.source="));
        assert!(dockerfile.contains("org.opencontainers.image.revision="));
        assert!(dockerfile.contains("sy.spark.base-manifest=\"sha256:"));
        assert!(dockerfile.contains("sy.spark.cuda-architecture="));
    }
}

#[test]
fn shipped_engine_profiles_declare_complete_runtime_policy() {
    let policies = engine_policies();
    assert!(!policies.is_empty());
    for policy in policies {
        assert_complete_runtime_policy(&policy);
    }
}

#[test]
fn config_derived_priorities_select_one_matching_control() {
    let policies = engine_policies();
    let candidates = contracted_engine_policies();
    assert!(!candidates.is_empty());
    for candidate in candidates {
        let priority = candidate["priority"].as_integer().unwrap();
        assert!(candidate["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .all(|profile| profile.get("sampling").is_some_and(toml::Value::is_table)));
        let selected_priority = policies
            .iter()
            .filter(|control| control["matcher"] == candidate["matcher"])
            .filter_map(|control| control["priority"].as_integer())
            .max()
            .unwrap();
        assert!(selected_priority > priority);
        assert_eq!(
            policies
                .iter()
                .filter(|control| control["matcher"] == candidate["matcher"])
                .filter(|control| control["priority"].as_integer() == Some(selected_priority))
                .count(),
            1
        );
        assert!(policies.iter().any(|control| {
            control["matcher"] == candidate["matcher"]
                && control["id"] != candidate["id"]
                && control["priority"]
                    .as_integer()
                    .is_some_and(|value| value > priority)
        }));
    }
}

#[test]
fn production_engine_dispatch_contains_no_configured_identifiers() {
    let production = engine_dispatch_sources();
    for identifier in configured_engine_and_model_identifiers() {
        assert!(
            !production.contains(&format!("\"{identifier}\"")),
            "production engine dispatch embeds configured identifier {identifier}"
        );
    }
}

#[test]
fn production_source_scan_discovers_new_modules_and_ignores_test_only_modules() {
    let fixture = tempfile::tempdir().unwrap();
    fs::write(
        fixture.path().join("mod.rs"),
        "mod future;\n#[cfg(all(feature = \"x\", test))]\npub mod fixture_only;\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("future.rs"),
        "production-sentinel\n#[cfg(test)]\nmod tests { test-section-sentinel }\n",
    )
    .unwrap();
    fs::write(fixture.path().join("fixture_only.rs"), "test-sentinel").unwrap();
    let production = engine_dispatch_sources_from(fixture.path());
    assert!(production.contains("production-sentinel"));
    assert!(!production.contains("test-sentinel"));
    assert!(!production.contains("test-section-sentinel"));
}
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[test]
fn all_engine_tomls_are_signed_without_family_names() {
    let fixture = tempfile::tempdir().unwrap();
    let binary = fixture.path().join("sy-aarch64");
    let release = fixture.path().join("release");
    fs::write(&binary, b"arm64 fixture").unwrap();
    assert!(Command::new("scripts/package-spark-release.sh")
        .args([&binary, &release])
        .status()
        .unwrap()
        .success());
    let manifest = fs::read_to_string(release.join("SHA256SUMS")).unwrap();
    let signed = manifest
        .lines()
        .filter_map(|line| line.split_once("  ").map(|(_, path)| path))
        .filter(|path| path.starts_with("configs/sy/spark/engines/"))
        .collect::<BTreeSet<_>>();
    let expected = engine_config_names()
        .into_iter()
        .map(|name| format!("configs/sy/spark/engines/{name}"))
        .collect::<BTreeSet<_>>();
    assert_eq!(signed, expected.iter().map(String::as_str).collect());
    assert!(Command::new("sha256sum")
        .args(["-c", "SHA256SUMS"])
        .current_dir(&release)
        .output()
        .unwrap()
        .status
        .success());
}

fn cfg_requires_test(attribute: &str) -> bool {
    let Some(expression) = attribute
        .strip_prefix("#[cfg(")
        .and_then(|value| value.strip_suffix(")]"))
    else {
        return false;
    };
    expression == "test"
        || (expression.starts_with("all(")
            && expression
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .any(|token| token == "test"))
}

fn declared_module_name(line: &str) -> Option<&str> {
    let declaration = line.strip_prefix("mod ").or_else(|| {
        line.split_once(" mod ")
            .filter(|(visibility, _)| visibility.starts_with("pub"))
            .map(|(_, declaration)| declaration)
    })?;
    declaration.strip_suffix(';')
}

fn production_module_names(source: &str) -> Vec<&str> {
    let mut attributes = Vec::new();
    let mut modules = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with("#[") {
            attributes.push(line);
            continue;
        }
        if let Some(name) = declared_module_name(line) {
            if !attributes.iter().any(|value| cfg_requires_test(value)) {
                modules.push(name);
            }
        }
        attributes.clear();
    }
    modules
}

fn source_without_bottom_test_module(source: &str) -> &str {
    let mut attributes = Vec::new();
    let mut attribute_start = None;
    let mut test_module_start = None;
    let mut offset = 0;
    for raw_line in source.split_inclusive('\n') {
        let line = raw_line.trim();
        if raw_line.starts_with("#[") {
            attribute_start.get_or_insert(offset);
            attributes.push(line);
        } else {
            if raw_line.starts_with("mod tests")
                && attributes.iter().any(|value| cfg_requires_test(value))
            {
                test_module_start = attribute_start;
            }
            attributes.clear();
            attribute_start = None;
        }
        offset += raw_line.len();
    }
    &source[..test_module_start.unwrap_or(source.len())]
}

fn engine_config_names() -> Vec<String> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "toml"))
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect()
}

fn model_catalog_names() -> Vec<String> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark");
    fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "toml"))
        .filter(|entry| {
            fs::read_to_string(entry.path())
                .ok()
                .and_then(|source| toml::from_str::<toml::Value>(&source).ok())
                .is_some_and(|value| value.get("models").is_some_and(toml::Value::is_array))
        })
        .map(|entry| entry.file_name().into_string().unwrap())
        .collect()
}

fn engine_dispatch_sources() -> String {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/spark");
    engine_dispatch_sources_from(&directory)
}

fn engine_dispatch_sources_from(directory: &Path) -> String {
    let modules = fs::read_to_string(directory.join("mod.rs")).unwrap();
    production_module_names(&modules)
        .into_iter()
        .map(|name| {
            let source = fs::read_to_string(directory.join(format!("{name}.rs"))).unwrap();
            source_without_bottom_test_module(&source).to_owned()
        })
        .collect()
}

fn insert_strings(identifiers: &mut BTreeSet<String>, values: Option<&Vec<toml::Value>>) {
    identifiers.extend(
        values
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned),
    );
}

fn configured_engine_identifiers() -> BTreeSet<String> {
    let mut identifiers = BTreeSet::new();
    for policy in engine_policies() {
        for key in ["id", "family"] {
            identifiers.insert(policy[key].as_str().unwrap().to_owned());
        }
    }
    identifiers
}

fn configured_model_identifiers() -> BTreeSet<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/models.toml");
    let document: toml::Value = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let mut identifiers = BTreeSet::new();
    for model in document["models"].as_array().unwrap() {
        insert_strings(&mut identifiers, model["aliases"].as_array());
        identifiers.insert(model["repository"].as_str().unwrap().to_owned());
        if let Some(profile) = model["artifact"]
            .get("engine_profile")
            .and_then(toml::Value::as_str)
        {
            identifiers.insert(profile.to_owned());
        }
    }
    identifiers
}

fn configured_engine_and_model_identifiers() -> BTreeSet<String> {
    let mut identifiers = configured_engine_identifiers();
    identifiers.extend(configured_model_identifiers());
    identifiers
}

fn nonempty_array<'a>(table: &'a toml::Value, key: &str) -> &'a [toml::Value] {
    let values = table[key].as_array().unwrap();
    assert!(!values.is_empty(), "{key} must not be empty");
    values
}

fn positive_integer(table: &toml::Value, key: &str) {
    assert!(table[key].as_integer().is_some_and(|value| value > 0));
}

fn assert_complete_runtime_policy(policy: &toml::Value) {
    nonempty_array(policy, "entrypoint");
    nonempty_array(policy, "arguments");
    nonempty_array(&policy["matcher"], "formats");
    if let Some(profiles) = policy["matcher"].get("engine_profiles") {
        assert!(!profiles.as_array().unwrap().is_empty());
    }
    for key in [
        "image_bytes",
        "startup_peak_bytes",
        "steady_peak_bytes",
        "compile_cache_bytes",
    ] {
        positive_integer(&policy["resources"], key);
    }
    assert_runtime_routes_and_health(policy);
    assert_runtime_profiles(policy);
    assert_isolation_policy(policy);
}

fn assert_runtime_routes_and_health(policy: &toml::Value) {
    for route in nonempty_array(policy, "routes") {
        assert!(route["method"].is_str() && route["path"].is_str());
    }
    assert!(policy["health_method"].is_str());
    assert!(policy["health_path"].is_str());
    assert!(policy["semantic_prompt"].is_str());
    positive_integer(policy, "semantic_max_tokens");
}

fn assert_runtime_profiles(policy: &toml::Value) {
    let profiles = nonempty_array(policy, "profiles");
    let default = policy["default_profile"].as_str().unwrap();
    assert!(profiles
        .iter()
        .any(|profile| profile["id"].as_str() == Some(default)));
    for profile in profiles {
        nonempty_array(profile, "arguments");
        nonempty_array(profile, "capabilities");
        positive_integer(profile, "context_window");
        assert!(profile.get("sampling").is_none_or(toml::Value::is_table));
    }
}

fn assert_isolation_policy(policy: &toml::Value) {
    for key in ["network", "seccomp", "ipc_mode", "compile_cache_root"] {
        assert!(policy[key].is_str(), "{key} must be configured");
    }
    for key in [
        "run_as_uid",
        "pid_limit",
        "shm_size_bytes",
        "startup_deadline_seconds",
    ] {
        positive_integer(policy, key);
    }
    nonempty_array(policy, "tmpfs");
    nonempty_array(policy, "environment");
}

fn engine_policies() -> Vec<toml::Value> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "toml"))
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .map(|source| toml::from_str(&source).unwrap())
        .collect()
}

fn contracted_engine_policies() -> Vec<toml::Value> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "Dockerfile")
        })
        .filter(|entry| {
            fs::read_to_string(entry.path())
                .is_ok_and(|source| source.contains("sy.spark.image-contract=\"v1\""))
        })
        .map(|entry| fs::read_to_string(entry.path().with_extension("toml")).unwrap())
        .map(|source| toml::from_str(&source).unwrap())
        .collect()
}

fn engine_dockerfiles() -> Vec<String> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/spark/engines");
    fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "Dockerfile")
        })
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .filter(|dockerfile| dockerfile.contains("sy.spark.image-contract=\"v1\""))
        .collect()
}
