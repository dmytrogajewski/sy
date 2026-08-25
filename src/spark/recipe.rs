//! Signed, root-owned Spark engine recipe catalog and deterministic selection.

use std::{collections::BTreeSet, path::Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::gateway::{EmbeddingPolicy, GatewayProfile, VisionPolicy};
use super::wire::{
    RecipeCatalogDocument, RecipeCompatibilityDocument, RecipeEvidenceDocument,
    RecipeMismatchDocument, RecipeResourceEnvelopeDocument, RecipeSelectionDocument,
    RecipeSelectionReason, RecipeStatus, RECIPE_CATALOG_SCHEMA,
};

const RECIPE_SCHEMA: &str = "sy.spark.recipe/v1";
pub const MAX_STARTUP_DEADLINE_SECONDS: u64 = 900;
const CATALOG_SCHEMA: &str = "sy.spark.recipe-catalog/v1";
const FIXED_HOST_ROOTS: [&str; 2] = [
    "/var/lib/sy-spark/huggingface",
    "/var/lib/sy-spark/compile-cache",
];
const ALLOWED_SUBSTITUTIONS: [&str; 5] = [
    "model_snapshot",
    "compile_cache",
    "port",
    "instance_id",
    "max_model_len",
];
const ORNITH_FILE: &str = "ornith-vllm.toml";
const QWEN_FILE: &str = "qwen3-embedding.toml";
const FIXTURE_FILE: &str = "fixture-http-echo.toml";
const SIGNED_RECIPES: [(&str, &[u8]); 3] = [
    (
        ORNITH_FILE,
        include_bytes!("../../configs/sy/spark/recipes/ornith-vllm.toml"),
    ),
    (
        QWEN_FILE,
        include_bytes!("../../configs/sy/spark/recipes/qwen3-embedding.toml"),
    ),
    (
        FIXTURE_FILE,
        include_bytes!("../../configs/sy/spark/recipes/fixture-http-echo.toml"),
    ),
];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub schema: String,
    pub identity: Identity,
    pub provenance: Provenance,
    pub host: HostMatch,
    pub model: ModelMatch,
    pub engine: Engine,
    pub isolation: Isolation,
    pub resources: ResourceEnvelope,
    pub health: Health,
    pub gateway: Gateway,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub id: String,
    pub version: u32,
    pub status: RecipeStatus,
    pub maintainer: String,
    pub source_url: String,
    pub source_commit: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub model_license: String,
    pub model_license_url: String,
    pub engine_license: String,
    pub engine_license_url: String,
    pub image_license: String,
    pub image_source_url: String,
    pub artifact_source_url: String,
    pub redistribution: String,
    pub acceptance_required: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostMatch {
    pub architecture: String,
    pub gpu_model: String,
    pub compute_capability: String,
    pub dgx_builds: Vec<String>,
    pub driver_min: String,
    pub driver_max_exclusive: String,
    pub toolkit_min: String,
    pub toolkit_max_exclusive: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelMatch {
    pub repository: String,
    pub commits: Vec<String>,
    pub format: String,
    pub precision: String,
    pub tokenizer_sha256: String,
    pub parser: String,
    pub parser_sha256: String,
    pub remote_code: RemoteCode,
    pub files: Vec<RequiredFile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteCode {
    pub allowed: bool,
    pub vendored_sha256: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    pub name: String,
    pub version: String,
    pub accelerator: Accelerator,
    pub image_repository: String,
    pub image_digest: String,
    pub image_architecture: String,
    pub entrypoint: Vec<String>,
    pub argv: Vec<String>,
    pub substitutions: Vec<Substitution>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Accelerator {
    Cpu,
    Nvidia,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Substitution {
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Isolation {
    pub network_policy: String,
    pub mounts: Vec<Mount>,
    pub writable_tmpfs: Vec<String>,
    pub capabilities: Vec<String>,
    pub seccomp: String,
    pub pid_limit: u32,
    pub run_as_uid: u32,
    pub disabled_features: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub host_root: String,
    pub container_path: String,
    pub read_only: bool,
    pub purpose: MountPurpose,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MountPurpose {
    Model,
    CompileCache,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceEnvelope {
    pub download_bytes: u64,
    pub image_bytes: u64,
    pub startup_peak_bytes: u64,
    pub steady_peak_bytes: u64,
    pub kv_cache_policy: String,
    pub context_ceiling: u64,
    pub concurrency_ceiling: u32,
    pub compile_cache_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub startup_deadline_seconds: u64,
    pub method: String,
    pub path: String,
    pub semantic_prompt: String,
    pub semantic_max_tokens: u32,
    pub served_model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Gateway {
    pub port: u16,
    pub methods: Vec<GatewayMethod>,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub vision: Option<VisionPolicy>,
    #[serde(default)]
    pub embeddings: Option<EmbeddingPolicy>,
}

impl Gateway {
    pub fn profile(&self) -> GatewayProfile {
        GatewayProfile {
            capabilities: self.capabilities.iter().cloned().collect(),
            vision: self.vision.clone(),
            embeddings: self.embeddings.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayMethod {
    pub method: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub host_fingerprint: String,
    pub image_run_as_uid: u32,
    pub upstream_recipe_commit: String,
    pub corpus: String,
    pub objective: String,
    pub quality: String,
    pub stability_seconds: u64,
    pub measured_metrics: Vec<String>,
    pub verified_at: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeHost {
    pub architecture: String,
    pub gpu_model: String,
    pub compute_capability: String,
    pub dgx_build: String,
    pub driver_version: String,
    pub toolkit_version: String,
    pub protected_fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct SelectionRequest<'a> {
    pub repository: &'a str,
    pub commit: &'a str,
    pub objective: &'a str,
    pub named_recipe: Option<&'a str>,
    pub allow_unverified: bool,
    pub tuned_winner: Option<TunedWinner<'a>>,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct TunedWinner<'a> {
    pub recipe_id: &'a str,
    pub fingerprint: &'a str,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct RecipeCatalog {
    recipes: Vec<Recipe>,
    digest: String,
}

impl RecipeCatalog {
    #[cfg(test)]
    pub fn signed_for_test() -> Self {
        Self::parse_documents(
            &SIGNED_RECIPES
                .iter()
                .map(|(name, bytes)| (*name, bytes.to_vec()))
                .collect::<Vec<_>>(),
        )
        .expect("embedded signed recipe catalog")
    }

    pub fn load_signed(directory: &Path) -> Result<Self, String> {
        let actual_names = std::fs::read_dir(directory)
            .map_err(|error| format!("read signed recipe catalog: {error}"))?
            .map(|entry| {
                entry
                    .map_err(|error| format!("read recipe catalog entry: {error}"))
                    .and_then(|entry| {
                        entry
                            .file_name()
                            .into_string()
                            .map_err(|_| "recipe filename is not UTF-8".to_owned())
                    })
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let expected_names = SIGNED_RECIPES
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect::<BTreeSet<_>>();
        if actual_names != expected_names {
            return Err("recipe catalog file set differs from the signed release".into());
        }
        let documents = SIGNED_RECIPES
            .iter()
            .map(|(name, signed)| {
                let bytes = std::fs::read(directory.join(name))
                    .map_err(|error| format!("read signed recipe {name}: {error}"))?;
                if bytes != *signed {
                    return Err(format!("recipe {name} differs from the signed release"));
                }
                Ok((*name, bytes))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Self::parse_documents(&documents)
    }

    pub fn signed_assets() -> &'static [(&'static str, &'static [u8])] {
        &SIGNED_RECIPES
    }

    pub fn recipe(&self, id: &str) -> Option<&Recipe> {
        self.recipes.iter().find(|recipe| recipe.identity.id == id)
    }

    fn parse_documents(documents: &[(&str, Vec<u8>)]) -> Result<Self, String> {
        let mut recipes = Vec::with_capacity(documents.len());
        let mut ids = BTreeSet::new();
        let mut digest = Sha256::new();
        let mut ordered = documents.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(name, _)| *name);
        for (name, bytes) in ordered {
            digest.update(name.as_bytes());
            digest.update([0]);
            digest.update(bytes);
            let text =
                std::str::from_utf8(bytes).map_err(|_| format!("recipe {name} is not UTF-8"))?;
            let recipe: Recipe =
                toml::from_str(text).map_err(|error| format!("parse recipe {name}: {error}"))?;
            validate_recipe(&recipe).map_err(|error| format!("recipe {name}: {error}"))?;
            if !ids.insert(recipe.identity.id.clone()) {
                return Err(format!("duplicate recipe ID {}", recipe.identity.id));
            }
            recipes.push(recipe);
        }
        recipes.sort_by(|left, right| left.identity.id.cmp(&right.identity.id));
        Ok(Self {
            recipes,
            digest: format!("sha256:{:x}", digest.finalize()),
        })
    }

    pub fn explain(
        &self,
        host: &RecipeHost,
        request: &SelectionRequest<'_>,
    ) -> RecipeCatalogDocument {
        let mut compatibility = self
            .recipes
            .iter()
            .filter(|recipe| recipe.model.repository == request.repository)
            .map(|recipe| {
                explain_recipe(recipe, host, request.commit, request.objective, request.now)
            })
            .collect::<Vec<_>>();
        compatibility.sort_by(|left, right| left.id.cmp(&right.id));
        let selection = select(&compatibility, &self.recipes, host, request);
        RecipeCatalogDocument {
            schema: RECIPE_CATALOG_SCHEMA.into(),
            catalog_sha256: self.digest.clone(),
            model_repository: Some(request.repository.into()),
            model_commit: Some(request.commit.into()),
            objective: request.objective.into(),
            selection,
            recipes: compatibility,
        }
    }

    pub fn list(&self, host: &RecipeHost, now: DateTime<Utc>) -> RecipeCatalogDocument {
        let mut recipes = self
            .recipes
            .iter()
            .map(|recipe| explain_recipe(recipe, host, &recipe.model.commits[0], "agent", now))
            .collect::<Vec<_>>();
        recipes.sort_by(|left, right| left.id.cmp(&right.id));
        RecipeCatalogDocument {
            schema: RECIPE_CATALOG_SCHEMA.into(),
            catalog_sha256: self.digest.clone(),
            model_repository: None,
            model_commit: None,
            objective: "agent".into(),
            selection: None,
            recipes,
        }
    }

    pub fn query(
        &self,
        host: &RecipeHost,
        repository: Option<&str>,
        commit: Option<&str>,
        objective: &str,
        now: DateTime<Utc>,
    ) -> RecipeCatalogDocument {
        let Some(repository) = repository else {
            return self.list(host, now);
        };
        let resolved_commit = commit.or_else(|| {
            self.recipes
                .iter()
                .find(|recipe| recipe.model.repository == repository)
                .and_then(|recipe| recipe.model.commits.first())
                .map(String::as_str)
        });
        let Some(commit) = resolved_commit else {
            return RecipeCatalogDocument {
                schema: RECIPE_CATALOG_SCHEMA.into(),
                catalog_sha256: self.digest.clone(),
                model_repository: Some(repository.into()),
                model_commit: None,
                objective: objective.into(),
                selection: None,
                recipes: Vec::new(),
            };
        };
        self.explain(
            host,
            &SelectionRequest {
                repository,
                commit,
                objective,
                named_recipe: None,
                allow_unverified: false,
                tuned_winner: None,
                now,
            },
        )
    }
}

fn validate_recipe(recipe: &Recipe) -> Result<(), String> {
    if recipe.schema != RECIPE_SCHEMA {
        return Err("unsupported schema".into());
    }
    validate_id(&recipe.identity.id)?;
    validate_https(&recipe.identity.source_url)?;
    validate_commit(&recipe.identity.source_commit)?;
    for url in [
        &recipe.provenance.model_license_url,
        &recipe.provenance.engine_license_url,
        &recipe.provenance.image_source_url,
        &recipe.provenance.artifact_source_url,
    ] {
        validate_https(url)?;
    }
    if recipe.host.architecture != "aarch64" || recipe.engine.image_architecture != "arm64" {
        return Err("Spark recipes require aarch64/arm64 identities".into());
    }
    if recipe.model.commits.is_empty() || recipe.model.files.is_empty() {
        return Err("model commit and required-file sets must not be empty".into());
    }
    for commit in &recipe.model.commits {
        validate_commit(commit)?;
    }
    for file in &recipe.model.files {
        validate_relative_path(&file.path)?;
        validate_sha256(&file.sha256)?;
    }
    validate_sha256(&recipe.model.tokenizer_sha256)?;
    validate_sha256(&recipe.model.parser_sha256)?;
    if recipe.model.remote_code.allowed || !recipe.model.remote_code.vendored_sha256.is_empty() {
        return Err("runtime remote code is prohibited".into());
    }
    if recipe.engine.image_repository.contains('@')
        || recipe
            .engine
            .image_repository
            .rsplit('/')
            .next()
            .is_some_and(|part| part.contains(':'))
    {
        return Err("image repository must not contain a tag or digest".into());
    }
    validate_digest(&recipe.engine.image_digest)?;
    if recipe.engine.entrypoint.is_empty() || recipe.engine.argv.is_empty() {
        return Err("entrypoint and argv must be fixed token arrays".into());
    }
    let substitutions = recipe
        .engine
        .substitutions
        .iter()
        .map(|substitution| substitution.name.as_str())
        .collect::<BTreeSet<_>>();
    if substitutions.len() != recipe.engine.substitutions.len()
        || substitutions
            .iter()
            .any(|name| !ALLOWED_SUBSTITUTIONS.contains(name))
        || recipe.engine.substitutions.iter().any(|substitution| {
            substitution.values.is_empty()
                || substitution.values.len() > 16
                || substitution.values.iter().any(|value| {
                    value.is_empty()
                        || value.len() > 128
                        || !value.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'_' | b'.' | b'/' | b':' | b'-')
                        })
                })
        })
    {
        return Err("substitutions must be unique, bounded, and allowlisted".into());
    }
    for token in recipe.engine.entrypoint.iter().chain(&recipe.engine.argv) {
        validate_argv_token(token, &substitutions)?;
    }
    if recipe
        .engine
        .argv
        .iter()
        .any(|token| token == "--trust-remote-code")
    {
        return Err("runtime remote code flag is prohibited".into());
    }
    if recipe.isolation.network_policy != "sy-spark-internal-v1"
        || !recipe.isolation.capabilities.is_empty()
        || recipe.isolation.seccomp != "default"
        || recipe.isolation.pid_limit == 0
        || recipe.isolation.run_as_uid == 0
    {
        return Err("isolation policy is not permitted".into());
    }
    if recipe.evidence.image_run_as_uid != recipe.isolation.run_as_uid {
        return Err("non-root UID differs from signed image identity evidence".into());
    }
    let mut mount_targets = BTreeSet::new();
    for mount in &recipe.isolation.mounts {
        let purpose_matches = match mount.purpose {
            MountPurpose::Model => mount.host_root == FIXED_HOST_ROOTS[0] && mount.read_only,
            MountPurpose::CompileCache => mount.host_root == FIXED_HOST_ROOTS[1],
        };
        if !purpose_matches
            || !mount.container_path.starts_with('/')
            || !mount_targets.insert(mount.container_path.as_str())
        {
            return Err("mount is outside fixed roots or makes model data writable".into());
        }
    }
    if recipe
        .isolation
        .writable_tmpfs
        .iter()
        .any(|path| path != "/tmp")
    {
        return Err("tmpfs paths must be absolute container paths".into());
    }
    if recipe.gateway.methods.is_empty()
        || recipe.gateway.methods.iter().any(|route| {
            !matches!(route.method.as_str(), "GET" | "POST") || !route.path.starts_with('/')
        })
    {
        return Err("gateway method allowlist is invalid".into());
    }
    validate_gateway(&recipe.gateway)?;
    if !(1..=MAX_STARTUP_DEADLINE_SECONDS).contains(&recipe.health.startup_deadline_seconds) {
        return Err("startup deadline is outside the signed safety ceiling".into());
    }
    validate_digest(&recipe.evidence.host_fingerprint)?;
    validate_commit(&recipe.evidence.upstream_recipe_commit)?;
    parse_timestamp(&recipe.evidence.verified_at)?;
    if let Some(expires) = &recipe.evidence.expires_at {
        parse_timestamp(expires)?;
    }
    Ok(())
}

fn validate_gateway(gateway: &Gateway) -> Result<(), String> {
    const CAPABILITIES: &[&str] = &[
        "fixture_health",
        "text_generation",
        "tool_calling",
        "vision",
        "text_embeddings",
    ];
    let capabilities = gateway.capabilities.iter().collect::<BTreeSet<_>>();
    let has = |name: &str| gateway.capabilities.iter().any(|value| value == name);
    let route = |method: &str, path: &str| {
        gateway
            .methods
            .iter()
            .any(|entry| entry.method == method && entry.path == path)
    };
    let has_completion_route = route("POST", "/v1/completions");
    let has_chat_route = route("POST", "/v1/chat/completions");
    if capabilities.len() != gateway.capabilities.len()
        || capabilities
            .iter()
            .any(|capability| !CAPABILITIES.contains(&capability.as_str()))
        || has("vision") != gateway.vision.is_some()
        || has("text_embeddings") != gateway.embeddings.is_some()
        || has("tool_calling") && !has("text_generation")
    {
        return Err("gateway capabilities and exact policies disagree".into());
    }
    if !has("fixture_health")
        && (!route("GET", "/v1/models")
            || has("text_generation") != (has_completion_route && has_chat_route)
            || !has("text_generation") && (has_completion_route || has_chat_route)
            || has("text_embeddings") != route("POST", "/v1/embeddings"))
    {
        return Err("gateway routes and verified capabilities disagree".into());
    }
    if let Some(vision) = &gateway.vision {
        validate_sha256(&vision.processor_sha256)?;
        validate_sha256(&vision.health_image_sha256)?;
        if vision.media_types.is_empty()
            || vision
                .media_types
                .iter()
                .any(|kind| !matches!(kind.as_str(), "image/png" | "image/jpeg" | "image/webp"))
            || vision.max_bytes == 0
            || vision.max_total_bytes < vision.max_bytes
            || vision.max_count == 0
            || vision.max_width == 0
            || vision.max_height == 0
        {
            return Err("vision policy is invalid".into());
        }
        super::gateway::vision_health_image(vision)
            .map_err(|_| "vision health fixture is invalid".to_owned())?;
    }
    if let Some(embeddings) = &gateway.embeddings {
        if embeddings.dimensions == 0
            || embeddings.max_batch == 0
            || embeddings.max_input_bytes == 0
            || !embeddings.normalized
            || embeddings.normalization_tolerance_ppm == 0
            || !route("POST", "/v1/embeddings")
        {
            return Err("embedding policy is invalid".into());
        }
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), String> {
    if !value.is_empty()
        && value.len() <= 96
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
    {
        Ok(())
    } else {
        Err("recipe ID is invalid".into())
    }
}

fn validate_https(value: &str) -> Result<(), String> {
    if value.starts_with("https://") && !value.bytes().any(|byte| byte.is_ascii_control()) {
        Ok(())
    } else {
        Err("provenance URLs must use HTTPS".into())
    }
}

fn validate_commit(value: &str) -> Result<(), String> {
    if value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("commit must be a full 40-character identity".into())
    }
}

fn validate_sha256(value: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("SHA-256 must contain exactly 64 hexadecimal characters".into())
    }
}

fn validate_digest(value: &str) -> Result<(), String> {
    value
        .strip_prefix("sha256:")
        .ok_or_else(|| "digest must use SHA-256".to_owned())
        .and_then(validate_sha256)
}

fn validate_relative_path(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        Err("model file path must be relative and traversal-free".into())
    } else {
        Ok(())
    }
}

fn validate_argv_token(token: &str, substitutions: &BTreeSet<&str>) -> Result<(), String> {
    if token.is_empty()
        || token
            .bytes()
            .any(|byte| byte == 0 || matches!(byte, b'\n' | b'\r'))
        || [";", "|", "`", "$(", "${", ">", "<"]
            .iter()
            .any(|part| token.contains(part))
    {
        return Err("argv contains shell syntax or a control character".into());
    }
    let mut rest = token;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let end = after
            .find('}')
            .ok_or_else(|| "argv contains an unterminated substitution".to_owned())?;
        let name = &after[..end];
        if !substitutions.contains(name) {
            return Err(format!("argv uses unknown substitution {name}"));
        }
        rest = &after[end + 1..];
    }
    if rest.contains('}') {
        return Err("argv contains an unmatched substitution delimiter".into());
    }
    Ok(())
}

fn explain_recipe(
    recipe: &Recipe,
    host: &RecipeHost,
    commit: &str,
    objective: &str,
    now: DateTime<Utc>,
) -> RecipeCompatibilityDocument {
    let mut mismatches = Vec::new();
    compare(
        "host.architecture",
        &host.architecture,
        &recipe.host.architecture,
        &mut mismatches,
    );
    compare(
        "host.gpu_model",
        &host.gpu_model,
        &recipe.host.gpu_model,
        &mut mismatches,
    );
    compare(
        "host.compute_capability",
        &host.compute_capability,
        &recipe.host.compute_capability,
        &mut mismatches,
    );
    if !recipe.host.dgx_builds.contains(&host.dgx_build) {
        mismatch(
            "host.dgx_build",
            &host.dgx_build,
            &recipe.host.dgx_builds.join(","),
            &mut mismatches,
        );
    }
    if !version_in_range(
        &host.driver_version,
        &recipe.host.driver_min,
        &recipe.host.driver_max_exclusive,
    ) {
        mismatch(
            "host.driver",
            &host.driver_version,
            "inside recipe range",
            &mut mismatches,
        );
    }
    if !version_in_range(
        &host.toolkit_version,
        &recipe.host.toolkit_min,
        &recipe.host.toolkit_max_exclusive,
    ) {
        mismatch(
            "host.container_toolkit",
            &host.toolkit_version,
            "inside recipe range",
            &mut mismatches,
        );
    }
    if !recipe.model.commits.iter().any(|allowed| allowed == commit) {
        mismatch(
            "model.commit",
            commit,
            &recipe.model.commits.join(","),
            &mut mismatches,
        );
    }
    if recipe.evidence.host_fingerprint != host.protected_fingerprint {
        mismatch(
            "evidence.host_fingerprint",
            &host.protected_fingerprint,
            &recipe.evidence.host_fingerprint,
            &mut mismatches,
        );
    }
    let expired = recipe
        .evidence
        .expires_at
        .as_deref()
        .and_then(|value| parse_timestamp(value).ok())
        .is_some_and(|expires| expires <= now);
    if expired {
        mismatch(
            "evidence.expires_at",
            &now.to_rfc3339(),
            "unexpired evidence",
            &mut mismatches,
        );
    }
    RecipeCompatibilityDocument {
        id: recipe.identity.id.clone(),
        version: recipe.identity.version,
        status: recipe.identity.status,
        model_repository: recipe.model.repository.clone(),
        model_commits: recipe.model.commits.clone(),
        engine: recipe.engine.name.clone(),
        engine_version: recipe.engine.version.clone(),
        image: format!(
            "{}@{}",
            recipe.engine.image_repository, recipe.engine.image_digest
        ),
        compatible: mismatches.is_empty() && recipe.identity.status != RecipeStatus::Disabled,
        mismatches,
        capabilities: recipe.gateway.capabilities.clone(),
        resources: RecipeResourceEnvelopeDocument {
            image_bytes: recipe.resources.image_bytes,
            startup_peak_bytes: recipe.resources.startup_peak_bytes,
            steady_peak_bytes: recipe.resources.steady_peak_bytes,
            compile_cache_bytes: recipe.resources.compile_cache_bytes,
        },
        evidence: RecipeEvidenceDocument {
            source_url: recipe.identity.source_url.clone(),
            source_commit: recipe.identity.source_commit.clone(),
            upstream_recipe_commit: recipe.evidence.upstream_recipe_commit.clone(),
            host_fingerprint: recipe.evidence.host_fingerprint.clone(),
            quality: recipe.evidence.quality.clone(),
            stability_seconds: recipe.evidence.stability_seconds,
            verified_at: recipe.evidence.verified_at.clone(),
            expires_at: recipe.evidence.expires_at.clone(),
        },
        remediation: remediation(recipe.identity.status, expired),
        fingerprint: recipe_fingerprint(recipe, host, objective).unwrap_or_default(),
        specialized_toggles: specialized_toggle_count(recipe),
    }
}

fn specialized_toggle_count(recipe: &Recipe) -> usize {
    recipe.engine.substitutions.len()
        + recipe
            .engine
            .argv
            .iter()
            .filter(|arg| {
                arg.starts_with("--enable-")
                    || arg.contains("speculative")
                    || arg.contains("flashinfer")
            })
            .count()
}

fn compare(
    field: &str,
    actual: &str,
    expected: &str,
    mismatches: &mut Vec<RecipeMismatchDocument>,
) {
    if actual != expected {
        mismatch(field, actual, expected, mismatches);
    }
}

fn mismatch(
    field: &str,
    actual: &str,
    expected: &str,
    mismatches: &mut Vec<RecipeMismatchDocument>,
) {
    mismatches.push(RecipeMismatchDocument {
        field: field.into(),
        actual: actual.into(),
        expected: expected.into(),
    });
}

fn remediation(status: RecipeStatus, expired: bool) -> Vec<String> {
    let mut result = Vec::new();
    if status == RecipeStatus::Experimental {
        result.push("name this exact recipe and acknowledge --allow-unverified".into());
    }
    if status == RecipeStatus::Disabled {
        result.push("select a reviewed enabled recipe".into());
    }
    if expired {
        result.push("re-run the full verification suite for this exact fingerprint".into());
    }
    result
}

fn select(
    compatibility: &[RecipeCompatibilityDocument],
    recipes: &[Recipe],
    host: &RecipeHost,
    request: &SelectionRequest<'_>,
) -> Option<RecipeSelectionDocument> {
    if let Some(named) = request.named_recipe {
        return compatibility
            .iter()
            .find(|candidate| candidate.id == named)
            .filter(|candidate| {
                candidate.compatible
                    && (candidate.status != RecipeStatus::Experimental || request.allow_unverified)
            })
            .and_then(|candidate| {
                let recipe = recipes
                    .iter()
                    .find(|recipe| recipe.identity.id == candidate.id)?;
                Some(RecipeSelectionDocument {
                    recipe_id: candidate.id.clone(),
                    reason: RecipeSelectionReason::NamedCompatible,
                    fingerprint: recipe_fingerprint(recipe, host, request.objective).ok()?,
                })
            });
    }
    if let Some(winner) = &request.tuned_winner {
        if winner.expires_at > request.now {
            if let Some(candidate) = compatibility
                .iter()
                .find(|candidate| candidate.id == winner.recipe_id && candidate.compatible)
            {
                let recipe = recipes
                    .iter()
                    .find(|recipe| recipe.identity.id == candidate.id)?;
                let fingerprint = recipe_fingerprint(recipe, host, request.objective).ok()?;
                if fingerprint == winner.fingerprint {
                    return Some(RecipeSelectionDocument {
                        recipe_id: candidate.id.clone(),
                        reason: RecipeSelectionReason::TunedWinner,
                        fingerprint,
                    });
                }
            }
        }
    }
    compatibility
        .iter()
        .filter(|candidate| {
            candidate.compatible
                && candidate.engine == "vllm"
                && matches!(
                    candidate.status,
                    RecipeStatus::LocalVerified | RecipeStatus::UpstreamVerified
                )
        })
        .min_by_key(|candidate| {
            (
                match candidate.status {
                    RecipeStatus::LocalVerified => 0,
                    RecipeStatus::UpstreamVerified => 1,
                    _ => 2,
                },
                candidate.id.as_str(),
            )
        })
        .and_then(|candidate| {
            let recipe = recipes
                .iter()
                .find(|recipe| recipe.identity.id == candidate.id)?;
            Some(RecipeSelectionDocument {
                recipe_id: candidate.id.clone(),
                reason: RecipeSelectionReason::VerifiedVllmFallback,
                fingerprint: recipe_fingerprint(recipe, host, request.objective).ok()?,
            })
        })
}

fn recipe_fingerprint(
    recipe: &Recipe,
    host: &RecipeHost,
    objective: &str,
) -> Result<String, String> {
    #[derive(Serialize)]
    struct Input<'a> {
        catalog_schema: &'static str,
        recipe: &'a Recipe,
        host: &'a RecipeHost,
        tokenizer_sha256: &'a str,
        parser_sha256: &'a str,
        corpus: &'a str,
        objective: &'a str,
    }
    let bytes = serde_json::to_vec(&Input {
        catalog_schema: CATALOG_SCHEMA,
        recipe,
        host,
        tokenizer_sha256: &recipe.model.tokenizer_sha256,
        parser_sha256: &recipe.model.parser_sha256,
        corpus: &recipe.evidence.corpus,
        objective,
    })
    .map_err(|error| format!("encode recipe fingerprint: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn version_in_range(actual: &str, minimum: &str, maximum_exclusive: &str) -> bool {
    let parse = |value: &str| {
        value
            .split('.')
            .map(|part| part.parse::<u64>())
            .collect::<Result<Vec<_>, _>>()
            .ok()
    };
    match (parse(actual), parse(minimum), parse(maximum_exclusive)) {
        (Some(actual), Some(minimum), Some(maximum)) => actual >= minimum && actual < maximum,
        _ => false,
    }
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| "evidence timestamp is not RFC3339".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_catalog() -> RecipeCatalog {
        RecipeCatalog::signed_for_test()
    }

    fn host() -> RecipeHost {
        RecipeHost {
            architecture: "aarch64".into(),
            gpu_model: "NVIDIA GB10".into(),
            compute_capability: "12.1".into(),
            dgx_build: "7.5.0".into(),
            driver_version: "580.159.03".into(),
            toolkit_version: "1.19.0".into(),
            protected_fingerprint:
                "sha256:7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510".into(),
        }
    }

    fn request<'a>(repository: &'a str, commit: &'a str) -> SelectionRequest<'a> {
        SelectionRequest {
            repository,
            commit,
            objective: "agent",
            named_recipe: None,
            allow_unverified: false,
            tuned_winner: None,
            now: "2026-08-24T00:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn strict_schema_rejects_every_unbounded_or_mutable_field() {
        let original = std::str::from_utf8(SIGNED_RECIPES[0].1).unwrap();
        for altered in [
            original.replace("schema =", "unknown = true\nschema ="),
            original.replace(
                "image_repository = \"vllm/vllm-openai\"",
                "image_repository = \"vllm/vllm-openai:latest\"",
            ),
            original.replace("read_only = true", "read_only = false"),
            original.replace("{port}", "{attacker_url}"),
            original.replace("allowed = false", "allowed = true"),
            original.replace(
                "host_root = \"/var/lib/sy-spark/huggingface\"",
                "host_root = \"/home/operator\"",
            ),
            original.replace(
                "\"--host\", \"0.0.0.0\"",
                "\"--host\", \"0.0.0.0;curl attacker\"",
            ),
        ] {
            assert!(
                RecipeCatalog::parse_documents(&[(ORNITH_FILE, altered.as_bytes().to_vec())])
                    .is_err(),
                "accepted adversarial recipe containing {}",
                altered
                    .lines()
                    .find(|line| {
                        line.contains("unknown")
                            || line.contains("latest")
                            || line.contains("read_only = false")
                            || line.contains("attacker")
                            || line.contains("allowed = true")
                            || line.contains("/home/operator")
                    })
                    .unwrap_or("unidentified mutation")
            );
        }
    }

    #[test]
    fn image_identity_evidence_binds_the_nonroot_uid() {
        let recipe = signed_catalog()
            .recipe("ornith-1.5-9b-vllm-0.19.1")
            .unwrap()
            .clone();
        assert_eq!(
            (
                recipe.isolation.run_as_uid,
                recipe.evidence.image_run_as_uid
            ),
            (65_534, 65_534)
        );
        let original = std::str::from_utf8(SIGNED_RECIPES[0].1).unwrap();
        let uid = original
            .lines()
            .find(|line| line.starts_with("run_as_uid = "))
            .unwrap();
        let altered = original.replacen(uid, "run_as_uid = 1", 1);
        assert!(RecipeCatalog::parse_documents(&[(ORNITH_FILE, altered.into_bytes())]).is_err());
    }

    #[test]
    fn full_fingerprint_changes_on_every_identity_input() {
        let catalog = signed_catalog();
        let recipe = &catalog.recipes[0];
        let baseline = recipe_fingerprint(recipe, &host(), "agent").unwrap();
        let mut changed = recipe.clone();
        changed.model.commits[0].replace_range(..1, "0");
        assert_ne!(
            recipe_fingerprint(&changed, &host(), "agent").unwrap(),
            baseline
        );
        let mut changed = recipe.clone();
        changed.engine.image_digest.replace_range(7..8, "0");
        assert_ne!(
            recipe_fingerprint(&changed, &host(), "agent").unwrap(),
            baseline
        );
        let mut changed = recipe.clone();
        changed.isolation.run_as_uid = 1;
        changed.evidence.image_run_as_uid = 1;
        assert_ne!(
            recipe_fingerprint(&changed, &host(), "agent").unwrap(),
            baseline
        );
        let mut changed_host = host();
        changed_host.driver_version = "580.160.0".into();
        assert_ne!(
            recipe_fingerprint(recipe, &changed_host, "agent").unwrap(),
            baseline
        );
        let mut changed = recipe.clone();
        changed.model.parser_sha256.replace_range(..1, "0");
        assert_ne!(
            recipe_fingerprint(&changed, &host(), "agent").unwrap(),
            baseline
        );
        let mut changed = recipe.clone();
        changed.evidence.corpus = "sy.spark.agent-corpus/v2".into();
        assert_ne!(
            recipe_fingerprint(&changed, &host(), "agent").unwrap(),
            baseline
        );
        assert_ne!(
            recipe_fingerprint(recipe, &host(), "throughput").unwrap(),
            baseline
        );
    }

    #[test]
    fn selection_uses_tuned_winner_then_verified_vllm_fallback() {
        let catalog = signed_catalog();
        let fallback = catalog.explain(
            &host(),
            &request(
                "ornith-ai/Ornith-1.5-9B",
                "489cb97981b8654bcfcf30ce1f94ed1b62e07b53",
            ),
        );
        let selected = fallback.selection.unwrap();
        assert_eq!(selected.reason, RecipeSelectionReason::VerifiedVllmFallback);
        let fingerprint = selected.fingerprint;
        let mut tuned = request(
            "ornith-ai/Ornith-1.5-9B",
            "489cb97981b8654bcfcf30ce1f94ed1b62e07b53",
        );
        tuned.tuned_winner = Some(TunedWinner {
            recipe_id: "ornith-1.5-9b-vllm-0.19.1",
            fingerprint: &fingerprint,
            expires_at: "2026-09-24T00:00:00Z".parse().unwrap(),
        });
        assert_eq!(
            catalog.explain(&host(), &tuned).selection.unwrap().reason,
            RecipeSelectionReason::TunedWinner
        );
    }

    #[test]
    fn untuned_ornith_selects_only_exact_verified_vllm() {
        let catalog = signed_catalog();
        let selected = catalog
            .explain(
                &host(),
                &request(
                    "ornith-ai/Ornith-1.5-9B",
                    "489cb97981b8654bcfcf30ce1f94ed1b62e07b53",
                ),
            )
            .selection
            .unwrap();
        let recipe = catalog.recipe(&selected.recipe_id).unwrap();

        assert_eq!(selected.reason, RecipeSelectionReason::VerifiedVllmFallback);
        assert_eq!(recipe.engine.name, "vllm");
        assert_eq!(recipe.model.precision, "bf16");
        assert_eq!(recipe.engine.substitutions[2].values, ["262144"]);
    }

    #[test]
    fn ornith_local_verification_is_bound_to_real_dgx_evidence() {
        const VERIFIED_AT: &str = "2026-08-24T16:45:32Z";
        const EXPIRES_AT: &str = "2027-08-24T16:45:32Z";
        let catalog = signed_catalog();
        let recipe = catalog.recipe("ornith-1.5-9b-vllm-0.19.1").unwrap();

        assert_eq!(recipe.identity.status, RecipeStatus::LocalVerified);
        assert_eq!(recipe.evidence.verified_at, VERIFIED_AT);
        assert_eq!(recipe.evidence.expires_at.as_deref(), Some(EXPIRES_AT));
        assert!(recipe
            .evidence
            .measured_metrics
            .contains(&"generation_11_startup_ms=297452".into()));
        assert!(recipe
            .evidence
            .measured_metrics
            .contains(&"generation_12_cached_re_serve_startup_ms=167217".into()));
        assert!(recipe
            .evidence
            .measured_metrics
            .contains(&"post_start_mem_available_bytes=54629040128".into()));
    }

    #[test]
    fn ornith_local_evidence_makes_no_stress_soak_or_secret_claim() {
        let catalog = signed_catalog();
        let recipe = catalog.recipe("ornith-1.5-9b-vllm-0.19.1").unwrap();
        let claims = format!(
            "{} {}",
            recipe.evidence.quality,
            recipe.evidence.measured_metrics.join(" ")
        )
        .to_ascii_lowercase();

        assert_eq!(recipe.evidence.stability_seconds, 0);
        assert!(
            ["stress", "soak", "throughput", "password", "bearer", "hf_"]
                .iter()
                .all(|forbidden| !claims.contains(forbidden))
        );
    }

    #[test]
    fn gb10_calibration_bounds_vllm_memory_without_reducing_full_context() {
        const GIB: u64 = 1_073_741_824;
        const MEASURED_KV_HUNDREDTH_GIB: u64 = 8_444;
        const MEASURED_DEFAULT_TOKENS: u64 = 691_680;
        const UNIFIED_MEMORY_GIB: u64 = 128;
        let recipe = signed_catalog()
            .recipe("ornith-1.5-9b-vllm-0.19.1")
            .unwrap()
            .clone();
        assert!(recipe
            .engine
            .argv
            .windows(2)
            .any(|pair| { pair == ["--gpu-memory-utilization", "0.5"] }));
        assert!(!recipe.engine.argv.iter().any(|token| token == "0.9"));
        assert_eq!(recipe.resources.startup_peak_bytes, 64 * GIB);
        assert_eq!(recipe.resources.steady_peak_bytes, 64 * GIB);
        let measured_kv = MEASURED_KV_HUNDREDTH_GIB * GIB / 100;
        let calibrated_kv = measured_kv - 4 * UNIFIED_MEMORY_GIB * GIB / 10;
        let estimated_tokens = MEASURED_DEFAULT_TOKENS * calibrated_kv / measured_kv;
        assert!(estimated_tokens >= recipe.resources.context_ceiling);
    }

    #[test]
    fn harmless_fixture_is_signed_named_only_and_does_not_replace_vllm_default() {
        let catalog = signed_catalog();
        let mut fixture = request(
            "ornith-ai/Ornith-1.5-9B",
            "489cb97981b8654bcfcf30ce1f94ed1b62e07b53",
        );
        fixture.named_recipe = Some("spark-fixture-http-echo-1.0.0");
        let named = catalog.explain(&host(), &fixture).selection.unwrap();
        let default = catalog
            .explain(
                &host(),
                &request(
                    "ornith-ai/Ornith-1.5-9B",
                    "489cb97981b8654bcfcf30ce1f94ed1b62e07b53",
                ),
            )
            .selection
            .unwrap();

        assert_eq!(named.reason, RecipeSelectionReason::NamedCompatible);
        assert_eq!(named.recipe_id, "spark-fixture-http-echo-1.0.0");
        assert_eq!(default.recipe_id, "ornith-1.5-9b-vllm-0.19.1");
    }

    #[test]
    fn verified_embedding_recipe_accepts_its_exact_name_without_acknowledgement() {
        let catalog = signed_catalog();
        let mut accepted = request(
            "Qwen/Qwen3-Embedding-0.6B",
            "97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3",
        );
        accepted.named_recipe = Some("qwen3-embedding-0.6b-vllm-0.19.1");
        assert_eq!(
            catalog
                .explain(&host(), &accepted)
                .selection
                .unwrap()
                .reason,
            RecipeSelectionReason::NamedCompatible
        );
    }

    #[test]
    fn qwen_local_verification_is_bound_to_real_dgx_evidence() {
        const VERIFIED_AT: &str = "2026-08-24T20:56:31Z";
        const EXPIRES_AT: &str = "2027-08-24T20:56:31Z";
        let catalog = signed_catalog();
        let recipe = catalog.recipe("qwen3-embedding-0.6b-vllm-0.19.1").unwrap();

        assert_eq!(recipe.identity.status, RecipeStatus::LocalVerified);
        assert_eq!(recipe.evidence.verified_at, VERIFIED_AT);
        assert_eq!(recipe.evidence.expires_at.as_deref(), Some(EXPIRES_AT));
        for fact in [
            "release_sha256=318e8eeb560a12021507f71c5b568c4bd01ccefb773fee86a3fe34c196f3b26f",
            "model_commit=97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3",
            "public_embedding_identity_order_dimension_normalization_usage=pass",
            "revoked_token_http_401=pass",
            "ornith_qwen_functional_coexistence_http_200=pass",
            "memory_floor_bytes=8589934592",
            "protected_fingerprint_unchanged=true",
        ] {
            assert!(recipe.evidence.measured_metrics.contains(&fact.into()));
        }
    }

    #[test]
    fn qwen_local_evidence_makes_no_repetition_stress_timing_or_secret_claim() {
        let catalog = signed_catalog();
        let recipe = catalog.recipe("qwen3-embedding-0.6b-vllm-0.19.1").unwrap();
        let claims = format!(
            "{} {}",
            recipe.evidence.quality,
            recipe.evidence.measured_metrics.join(" ")
        )
        .to_ascii_lowercase();

        assert_eq!(recipe.evidence.stability_seconds, 0);
        assert!([
            "stress",
            "soak",
            "throughput",
            "repeat",
            "determin",
            "timing",
            "latency",
            "password",
            "bearer",
            "hf_",
        ]
        .iter()
        .all(|forbidden| !claims.contains(forbidden)));
    }

    #[test]
    fn advertised_routes_equal_verified_capabilities() {
        let catalog = signed_catalog();
        let ornith = catalog.recipe("ornith-1.5-9b-vllm-0.19.1").unwrap();
        let embedding = catalog.recipe("qwen3-embedding-0.6b-vllm-0.19.1").unwrap();

        assert!(ornith.gateway.profile().vision.is_some());
        assert!(embedding.gateway.profile().embeddings.is_some());
        assert!(!embedding
            .gateway
            .profile()
            .capabilities
            .contains("text_generation"));
        let mut missing_route = ornith.gateway.clone();
        missing_route
            .methods
            .retain(|route| route.path != "/v1/chat/completions");
        assert!(validate_gateway(&missing_route).is_err());
        let mut engine_advertisement = embedding.gateway.clone();
        engine_advertisement.methods.push(GatewayMethod {
            method: "POST".into(),
            path: "/v1/completions".into(),
        });
        assert!(validate_gateway(&engine_advertisement).is_err());
    }

    #[test]
    fn qwen_embedding_uses_the_pinned_vllm_pooling_runner_cli() {
        const GIB: u64 = 1_073_741_824;
        const VERIFIED_DEVICE_MEMORY_HUNDREDTH_GIB: u64 = 11_961;
        let catalog = signed_catalog();
        let recipe = catalog.recipe("qwen3-embedding-0.6b-vllm-0.19.1").unwrap();

        assert!(recipe
            .engine
            .argv
            .windows(2)
            .any(|pair| pair == ["--runner", "pooling"]));
        assert!(!recipe
            .engine
            .argv
            .iter()
            .any(|argument| argument == "--task"));
        assert!(recipe
            .engine
            .argv
            .windows(2)
            .any(|pair| pair == ["--gpu-memory-utilization", "0.06"]));
        let bounded_engine_bytes = VERIFIED_DEVICE_MEMORY_HUNDREDTH_GIB * GIB * 6 / 10_000;
        assert!(bounded_engine_bytes <= recipe.resources.steady_peak_bytes);
        assert_eq!(recipe.resources.steady_peak_bytes, 8 * GIB);
        assert_eq!(recipe.resources.startup_peak_bytes, 8 * GIB);
    }

    #[test]
    fn signed_catalog_rejects_changed_or_extra_files() {
        let root = tempfile::tempdir().unwrap();
        for (name, bytes) in SIGNED_RECIPES {
            std::fs::write(root.path().join(name), bytes).unwrap();
        }
        assert!(RecipeCatalog::load_signed(root.path()).is_ok());
        std::fs::write(root.path().join(ORNITH_FILE), b"changed").unwrap();
        assert!(RecipeCatalog::load_signed(root.path()).is_err());
        std::fs::write(root.path().join(ORNITH_FILE), SIGNED_RECIPES[0].1).unwrap();
        std::fs::write(root.path().join("extra.toml"), b"schema='x'").unwrap();
        assert!(RecipeCatalog::load_signed(root.path()).is_err());
    }
}
