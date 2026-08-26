//! Runtime-loaded Spark inference-engine policy.

use std::{collections::BTreeSet, path::Path};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{gateway::GatewayProfile, wire::RecipeResourceEnvelopeDocument};

pub const ENGINE_SCHEMA: &str = "sy.spark.engine/v1";
const ALLOWED_PLACEHOLDERS: [&str; 3] = ["{model_snapshot}", "{served_model}", "{port}"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub schema: String,
    pub id: String,
    pub family: String,
    pub version: String,
    pub image_repository: String,
    pub image_digest: String,
    pub image_architecture: String,
    pub entrypoint: Vec<String>,
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: Vec<String>,
    pub default_profile: String,
    pub model_cache_root: String,
    pub compile_cache_root: String,
    pub network: String,
    pub port: u16,
    pub run_as_uid: u32,
    pub pid_limit: u32,
    pub seccomp: String,
    pub tmpfs: Vec<String>,
    pub startup_deadline_seconds: u64,
    pub health_method: String,
    pub health_path: String,
    pub semantic_prompt: String,
    pub semantic_max_tokens: u32,
    pub resources: EngineResources,
    pub routes: Vec<EngineRoute>,
    pub profiles: Vec<EngineProfile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineResources {
    pub image_bytes: u64,
    pub startup_peak_bytes: u64,
    pub steady_peak_bytes: u64,
    pub compile_cache_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineRoute {
    pub method: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineProfile {
    pub id: String,
    pub model_types: Vec<String>,
    pub arguments: Vec<String>,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub sampling: EngineSampling,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineSampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub min_p: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub repetition_penalty: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct EnginePolicy {
    config: EngineConfig,
    fingerprint: String,
}

impl EnginePolicy {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("read Spark engine policy: {error}"))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let config: EngineConfig =
            toml::from_str(text).map_err(|error| format!("parse Spark engine policy: {error}"))?;
        validate(&config)?;
        Ok(Self {
            config,
            fingerprint: format!("sha256:{:x}", Sha256::digest(text.as_bytes())),
        })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn profile(&self, model_type: Option<&str>) -> &EngineProfile {
        model_type
            .and_then(|model_type| {
                self.config
                    .profiles
                    .iter()
                    .find(|profile| profile.model_types.iter().any(|item| item == model_type))
            })
            .or_else(|| {
                self.config
                    .profiles
                    .iter()
                    .find(|profile| profile.id == self.config.default_profile)
            })
            .expect("validated engine policy has a default profile")
    }

    pub fn image(&self) -> String {
        format!(
            "{}@{}",
            self.config.image_repository, self.config.image_digest
        )
    }

    pub fn resources(&self) -> RecipeResourceEnvelopeDocument {
        RecipeResourceEnvelopeDocument {
            image_bytes: self.config.resources.image_bytes,
            startup_peak_bytes: self.config.resources.startup_peak_bytes,
            steady_peak_bytes: self.config.resources.steady_peak_bytes,
            compile_cache_bytes: self.config.resources.compile_cache_bytes,
        }
    }

    pub fn gateway_profile(&self, model_type: Option<&str>) -> GatewayProfile {
        let profile = self.profile(model_type);
        GatewayProfile {
            capabilities: profile.capabilities.iter().cloned().collect(),
            vision: None,
            embeddings: None,
            sampling: sampling_policy(&profile.sampling),
        }
    }
}

fn sampling_policy(config: &EngineSampling) -> super::gateway::SamplingPolicy {
    let mut defaults = std::collections::BTreeMap::new();
    for (key, value) in [
        ("temperature", config.temperature),
        ("top_p", config.top_p),
        ("min_p", config.min_p),
        ("presence_penalty", config.presence_penalty),
        ("repetition_penalty", config.repetition_penalty),
    ] {
        if let Some(value) = value.and_then(serde_json::Number::from_f64) {
            defaults.insert(key.into(), value);
        }
    }
    if let Some(value) = config.top_k {
        defaults.insert("top_k".into(), value.into());
    }
    super::gateway::SamplingPolicy { defaults }
}

fn validate(config: &EngineConfig) -> Result<(), String> {
    if config.schema != ENGINE_SCHEMA {
        return Err("unsupported Spark engine policy schema".into());
    }
    if !valid_identifier(&config.id) || !valid_identifier(&config.family) {
        return Err("engine id and family must be bounded identifiers".into());
    }
    if config.version.is_empty()
        || config.image_repository.is_empty()
        || !valid_digest(&config.image_digest)
        || !valid_identifier(&config.image_architecture)
    {
        return Err("engine image identity is invalid".into());
    }
    if config.entrypoint.is_empty()
        || config.entrypoint.iter().any(|value| !valid_argument(value))
        || config.arguments.iter().any(|value| !valid_argument(value))
        || config
            .environment
            .iter()
            .any(|value| !valid_environment(value))
    {
        return Err("engine command contains an invalid argument".into());
    }
    for argument in config.entrypoint.iter().chain(&config.arguments) {
        if argument.contains('{') && !ALLOWED_PLACEHOLDERS.contains(&argument.as_str()) {
            return Err("engine command contains an unknown placeholder".into());
        }
    }
    if !valid_managed_root(&config.model_cache_root)
        || !valid_managed_root(&config.compile_cache_root)
        || config.model_cache_root == config.compile_cache_root
        || !valid_identifier(&config.network)
        || matches!(config.network.as_str(), "bridge" | "host" | "none")
        || config.port == 0
        || config.run_as_uid == 0
        || config.pid_limit == 0
        || !valid_identifier(&config.seccomp)
        || config.seccomp == "unconfined"
        || config.tmpfs != ["/tmp"]
        || !(1..=900).contains(&config.startup_deadline_seconds)
    {
        return Err("engine isolation policy is invalid".into());
    }
    if config.resources.image_bytes == 0
        || config.resources.startup_peak_bytes == 0
        || config.resources.steady_peak_bytes == 0
        || config.resources.compile_cache_bytes == 0
        || config.semantic_prompt.trim().is_empty()
        || config.semantic_max_tokens == 0
        || config.health_method != "GET"
        || !valid_route(&config.health_path)
    {
        return Err("engine resource or health policy is invalid".into());
    }
    if config.routes.is_empty()
        || config.routes.iter().any(|route| {
            !matches!(route.method.as_str(), "GET" | "POST") || !valid_route(&route.path)
        })
    {
        return Err("engine gateway route policy is invalid".into());
    }
    validate_profiles(config)
}

fn validate_profiles(config: &EngineConfig) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    let mut model_types = BTreeSet::new();
    for profile in &config.profiles {
        if !valid_identifier(&profile.id)
            || !ids.insert(&profile.id)
            || profile
                .arguments
                .iter()
                .any(|value| !valid_argument(value) || value.contains('{'))
            || profile.capabilities.is_empty()
            || profile.capabilities.iter().any(|capability| {
                !matches!(capability.as_str(), "text_generation" | "tool_calling")
            })
            || profile
                .model_types
                .iter()
                .any(|model_type| !valid_identifier(model_type) || !model_types.insert(model_type))
        {
            return Err("engine profile is invalid or ambiguous".into());
        }
        validate_sampling(&profile.sampling)?;
    }
    ids.contains(&config.default_profile)
        .then_some(())
        .ok_or_else(|| "engine default profile does not exist".into())
}

fn validate_sampling(sampling: &EngineSampling) -> Result<(), String> {
    let bounded = |value: Option<f64>, minimum: f64, maximum: f64| {
        value.is_none_or(|value| value.is_finite() && (minimum..=maximum).contains(&value))
    };
    if bounded(sampling.temperature, 0.0, 2.0)
        && bounded(sampling.top_p, 0.0, 1.0)
        && bounded(sampling.min_p, 0.0, 1.0)
        && bounded(sampling.presence_penalty, -2.0, 2.0)
        && bounded(sampling.repetition_penalty, 0.0, 2.0)
        && sampling.top_k.is_none_or(|value| value > 0)
    {
        Ok(())
    } else {
        Err("engine profile sampling policy is invalid".into())
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_managed_root(value: &str) -> bool {
    let path = Path::new(value);
    path.is_absolute()
        && path.starts_with("/var/lib/sy-spark")
        && path != Path::new("/var/lib/sy-spark")
        && !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn valid_argument(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value
            .bytes()
            .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
}

fn valid_environment(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, value)| {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && valid_argument(value)
    })
}

fn valid_route(value: &str) -> bool {
    value.starts_with('/')
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = include_str!("../../configs/sy/spark/engine.toml");

    #[test]
    fn operational_values_live_only_in_configuration() {
        let policy = EnginePolicy::parse(POLICY).unwrap();
        let mut values = vec![
            policy.config.id.as_str(),
            policy.config.version.as_str(),
            policy.config.image_repository.as_str(),
            policy.config.image_digest.as_str(),
            policy.config.model_cache_root.as_str(),
            policy.config.compile_cache_root.as_str(),
            policy.config.network.as_str(),
        ];
        values.extend(policy.config.entrypoint.iter().map(String::as_str));
        values.extend(policy.config.arguments.iter().map(String::as_str));
        values.extend(policy.config.environment.iter().map(String::as_str));
        values.extend(policy.config.profiles.iter().flat_map(|profile| {
            profile
                .model_types
                .iter()
                .map(String::as_str)
                .chain(profile.arguments.iter().map(String::as_str))
        }));
        let runtime = ["engine.rs", "executor.rs", "gateway.rs", "agent.rs"]
            .into_iter()
            .map(|name| {
                let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/spark")
                    .join(name);
                let source = std::fs::read_to_string(path).unwrap();
                source
                    .split("\n#[cfg(test)]\nmod tests")
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for value in values
            .into_iter()
            .filter(|value| value.len() >= 7 && !value.contains('{'))
        {
            assert!(!runtime.contains(value), "runtime source embeds {value}");
        }
        for value in [
            policy.config.resources.image_bytes,
            policy.config.resources.startup_peak_bytes,
            policy.config.resources.steady_peak_bytes,
            policy.config.resources.compile_cache_bytes,
        ] {
            assert!(
                !runtime.contains(&value.to_string()),
                "runtime source embeds resource value {value}"
            );
        }
        for forbidden in ["ORNITH_", "QWEN_", "0.6", "0.95"] {
            assert!(
                !runtime.contains(forbidden),
                "runtime source embeds model policy {forbidden}"
            );
        }
    }

    #[test]
    fn config_selects_profiles_from_model_metadata() {
        let policy = EnginePolicy::parse(POLICY).unwrap();
        assert_eq!(policy.profile(Some("qwen3_5")).id, "qwen3_5");
        assert_eq!(policy.profile(Some("llama")).id, "text");
    }

    #[test]
    fn qwen35_profile_serializes_mixed_gdn_batches() {
        let policy = EnginePolicy::parse(POLICY).unwrap();
        let profile = policy.profile(Some("qwen3_5"));
        assert!(profile
            .arguments
            .windows(2)
            .any(|pair| pair == ["--max-num-seqs", "1"]));
    }

    #[test]
    fn qwen3_profile_preserves_parallel_serving() {
        let policy = EnginePolicy::parse(POLICY).unwrap();
        let profile = policy.profile(Some("qwen3"));
        assert!(profile
            .arguments
            .windows(2)
            .any(|pair| pair == ["--max-num-seqs", "16"]));
    }

    #[test]
    fn config_rejects_unbounded_profile_arguments() {
        let invalid = POLICY.replacen(
            "arguments = [\"--max-num-seqs\", \"16\"]",
            "arguments = [\"line\\nbreak\"]",
            1,
        );
        assert!(EnginePolicy::parse(&invalid).is_err());
    }

    #[test]
    fn preceding_policy_without_environment_remains_upgrade_readable() {
        let previous = POLICY
            .lines()
            .skip_while(|line| *line != "environment = [")
            .skip(1)
            .position(|line| line == "]")
            .map(|end| {
                let lines = POLICY.lines().collect::<Vec<_>>();
                let start = lines
                    .iter()
                    .position(|line| *line == "environment = [")
                    .unwrap();
                lines
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, line)| {
                        (!(start..=start + end + 1).contains(&index)).then_some(line)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap();
        let policy = EnginePolicy::parse(&previous).unwrap();
        assert!(policy.config().environment.is_empty());
    }
}
