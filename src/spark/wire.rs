//! Strict JSON contracts shared by the Spark bootstrap and workstation client.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const INVENTORY_SCHEMA: &str = "sy.spark.bootstrap.inventory/v1";
pub const MANIFEST_SCHEMA: &str = "sy.spark.install-manifest/v1";
pub const FINGERPRINT_SCHEMA: &str = "sy.spark.protected-fingerprint/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ModelArtifactFormat {
    Gguf,
    Safetensors,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub enum ModelArtifactRole {
    Projector,
    WeightShard,
    Custom(String),
}

impl ModelArtifactRole {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Projector => "projector",
            Self::WeightShard => "weight_shard",
            Self::Custom(value) => value,
        }
    }
}

impl std::str::FromStr for ModelArtifactRole {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "projector" => Ok(Self::Projector),
            "weight_shard" => Ok(Self::WeightShard),
            _ if !value.is_empty()
                && value.len() <= 64
                && value.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                }) =>
            {
                Ok(Self::Custom(value.into()))
            }
            _ => Err("artifact role must be a lowercase identifier".into()),
        }
    }
}

impl Serialize for ModelArtifactRole {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ModelArtifactRole {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelArtifactFileDocument {
    pub path: String,
    pub bytes: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelAuxiliaryArtifactDocument {
    pub role: ModelArtifactRole,
    pub path: String,
    pub bytes: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelArtifactSelectorDocument {
    pub role: ModelArtifactRole,
    pub path: String,
}

impl std::str::FromStr for ModelArtifactSelectorDocument {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (role, path) = value
            .split_once('=')
            .ok_or_else(|| "auxiliary artifact must use ROLE=PATH".to_owned())?;
        if path.is_empty() || path.contains('=') {
            return Err("auxiliary artifact must contain one non-empty path".into());
        }
        Ok(Self {
            role: role.parse()?,
            path: path.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelArtifactsDocument {
    pub schema: String,
    pub format: ModelArtifactFormat,
    pub primary: ModelArtifactFileDocument,
    pub auxiliary: Vec<ModelAuxiliaryArtifactDocument>,
    pub quantization: Option<String>,
    pub capabilities: Vec<String>,
    pub configured_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_profile: Option<String>,
}

pub fn artifact_fingerprint(artifacts: &ModelArtifactsDocument) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let encoded = serde_json::to_vec(artifacts)
        .map_err(|error| format!("encode Spark artifact identity: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

#[cfg(feature = "spark-agent")]
pub const STATUS_SCHEMA: &str = "sy.spark.status/v1";
#[cfg(feature = "spark-agent")]
pub const DOCTOR_SCHEMA: &str = "sy.spark.doctor/v1";
#[cfg(feature = "spark-agent")]
pub const CERTIFICATE_SCHEMA: &str = "sy.spark.certificate-status/v1";
#[cfg(feature = "spark-agent")]
pub const PROBLEM_SCHEMA: &str = "sy.spark.problem/v1";
#[cfg(any(feature = "spark-agent", test))]
pub const OPERATION_SCHEMA: &str = "sy.spark.operation/v1";
#[cfg(feature = "spark-agent")]
pub const OPERATION_LIST_SCHEMA: &str = "sy.spark.operation-list/v1";
#[cfg(any(feature = "spark-agent", test))]
pub const OPERATION_EVENT_SCHEMA: &str = "sy.spark.operation-event/v1";
#[cfg(feature = "spark-agent")]
pub const TOKEN_SCHEMA: &str = "sy.spark.token/v1";
#[cfg(feature = "spark-agent")]
pub const TOKEN_LIST_SCHEMA: &str = "sy.spark.token-list/v1";
#[cfg(feature = "spark-agent")]
#[cfg(all(feature = "spark-agent", test))]
pub const RECIPE_CATALOG_SCHEMA: &str = "sy.spark.recipe-catalog/v1";
#[cfg(feature = "spark-agent")]
pub const MODEL_SCHEMA: &str = "sy.spark.model/v1";
#[cfg(feature = "spark-agent")]
pub const MODEL_LIST_SCHEMA: &str = "sy.spark.model-list/v1";
#[cfg(feature = "spark-agent")]
pub const REMOVAL_PLAN_SCHEMA: &str = "sy.spark.removal-plan/v1";
#[cfg(feature = "spark-agent")]
pub const INSTANCE_SCHEMA: &str = "sy.spark.instance/v2";
#[cfg(feature = "spark-agent")]
pub const INSTANCE_LIST_SCHEMA: &str = "sy.spark.instance-list/v1";
#[cfg(feature = "spark-agent")]
pub const ENGINE_LOG_SCHEMA: &str = "sy.spark.engine-log/v1";
#[cfg(all(feature = "spark-agent", test))]
pub const COMPATIBILITY_EVALUATION_SCHEMA: &str = "sy.spark.compatibility-evaluation/v1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[cfg(feature = "spark-agent")]
#[serde(untagged)]
pub enum OpenAiEmbeddingInput {
    String(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[cfg(feature = "spark-agent")]
#[serde(deny_unknown_fields)]
pub struct OpenAiEmbeddingRequest {
    pub model: String,
    pub input: OpenAiEmbeddingInput,
    pub encoding_format: Option<String>,
    pub dimensions: Option<usize>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[cfg(feature = "spark-agent")]
#[serde(deny_unknown_fields)]
pub struct OpenAiEmbeddingVector {
    pub object: String,
    pub index: usize,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[cfg(feature = "spark-agent")]
#[serde(deny_unknown_fields)]
pub struct OpenAiEmbeddingUsage {
    pub prompt_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[cfg(feature = "spark-agent")]
#[serde(deny_unknown_fields)]
pub struct OpenAiEmbeddingDocument {
    pub object: String,
    pub model: String,
    pub data: Vec<OpenAiEmbeddingVector>,
    pub usage: OpenAiEmbeddingUsage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[cfg(feature = "spark-agent")]
#[serde(deny_unknown_fields)]
pub struct AnthropicTokenCountDocument {
    pub input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[cfg(feature = "spark-agent")]
#[serde(deny_unknown_fields)]
pub struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[cfg(feature = "spark-agent")]
#[serde(deny_unknown_fields)]
pub struct AnthropicErrorDocument {
    #[serde(rename = "type")]
    pub kind: String,
    pub error: AnthropicErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DegradedReason {
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct StatusDocument {
    pub schema: String,
    pub agent: String,
    pub executor: String,
    pub read_only: bool,
    pub degraded_reasons: Vec<DegradedReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseHealth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_health: Option<ExecutorSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct DatabaseHealth {
    pub available: bool,
    pub journal_mode: String,
    pub synchronous: String,
    pub foreign_keys: bool,
    pub backup_valid: bool,
    pub queue_capacity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct ExecutorHealth {
    pub schema: String,
    pub version: String,
    pub authorized_agent_uid: u32,
    pub guard_heartbeat: bool,
    pub event_heartbeat: bool,
    #[serde(default)]
    pub event_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct ProtectedHostSnapshot {
    pub schema: String,
    pub hostname: String,
    pub kernel_release: String,
    pub architecture: String,
    pub identity_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct DockerCapability {
    pub schema: String,
    pub transport: String,
    pub version: String,
    pub api_version: String,
    pub minimum_api_version: String,
    pub os: String,
    pub architecture: String,
    pub experimental: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct ExecutorSnapshot {
    pub health: ExecutorHealth,
    pub host: ProtectedHostSnapshot,
    pub docker: DockerCapability,
    pub resources: super::resources::HostResourceSnapshot,
    pub resource_policy: super::resources::ResourcePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(feature = "spark-agent")]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(rename_all = "kebab-case")]
#[cfg(all(feature = "spark-agent", test))]
pub enum RecipeStatus {
    LocalVerified,
    UpstreamVerified,
    Experimental,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(feature = "spark-agent")]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[cfg(all(feature = "spark-agent", test))]
pub enum RecipeSelectionReason {
    NamedCompatible,
    TunedWinner,
    VerifiedVllmFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(feature = "spark-agent")]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
#[cfg(all(feature = "spark-agent", test))]
pub struct RecipeMismatchDocument {
    pub field: String,
    pub actual: String,
    pub expected: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(feature = "spark-agent")]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
#[cfg(all(feature = "spark-agent", test))]
pub struct RecipeEvidenceDocument {
    pub source_url: String,
    pub source_commit: String,
    pub upstream_recipe_commit: String,
    pub host_fingerprint: String,
    pub quality: String,
    pub stability_seconds: u64,
    pub verified_at: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(feature = "spark-agent")]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
#[cfg(all(feature = "spark-agent", test))]
pub struct RecipeCompatibilityDocument {
    pub id: String,
    pub version: u32,
    pub status: RecipeStatus,
    pub model_repository: String,
    pub model_commits: Vec<String>,
    pub engine: String,
    pub engine_version: String,
    pub image: String,
    pub compatible: bool,
    pub mismatches: Vec<RecipeMismatchDocument>,
    pub capabilities: Vec<String>,
    pub resources: RecipeResourceEnvelopeDocument,
    pub evidence: RecipeEvidenceDocument,
    pub remediation: Vec<String>,
    pub fingerprint: String,
    pub specialized_toggles: usize,
}

#[cfg(all(test, feature = "spark-agent"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Eligible,
    Rejected,
    Unsupported,
    Uninstalled,
    Selected,
}

#[cfg(all(test, feature = "spark-agent"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct FunctionalGateDocument {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[cfg(all(test, feature = "spark-agent"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct CandidateEvaluationDocument {
    pub engine_family: String,
    pub recipe_id: Option<String>,
    pub fingerprint: Option<String>,
    pub status: CandidateStatus,
    pub capability_tier: usize,
    pub specialized_toggles: usize,
    pub gates: Vec<FunctionalGateDocument>,
    pub reason: String,
}

#[cfg(all(test, feature = "spark-agent"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct CompatibilityEvaluationDocument {
    pub schema: String,
    pub id: String,
    pub model_id: String,
    pub repository: String,
    pub commit: String,
    pub objective: String,
    pub selected_recipe_id: Option<String>,
    pub selected_fingerprint: Option<String>,
    pub fallback_recipe_id: Option<String>,
    pub candidates: Vec<CandidateEvaluationDocument>,
    pub created_at: String,
    pub invalidated_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RecipeResourceEnvelopeDocument {
    pub image_bytes: u64,
    pub startup_peak_bytes: u64,
    pub steady_peak_bytes: u64,
    pub compile_cache_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(feature = "spark-agent")]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
#[cfg(all(feature = "spark-agent", test))]
pub struct RecipeSelectionDocument {
    pub recipe_id: String,
    pub reason: RecipeSelectionReason,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg(feature = "spark-agent")]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
#[cfg(all(feature = "spark-agent", test))]
pub struct RecipeCatalogDocument {
    pub schema: String,
    pub catalog_sha256: String,
    pub model_repository: Option<String>,
    pub model_commit: Option<String>,
    pub objective: String,
    pub selection: Option<RecipeSelectionDocument>,
    pub recipes: Vec<RecipeCompatibilityDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DownloadRequest {
    pub repository: String,
    #[serde(default = "default_model_revision")]
    pub revision: String,
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auxiliary: Vec<ModelArtifactSelectorDocument>,
    #[serde(default)]
    pub update_alias: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ServeAdmissionRequest {
    pub model: String,
    pub name: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ServeRequest {
    pub model: String,
    pub name: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct StopRequest {
    #[serde(default = "default_stop_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub dry_run: bool,
}

fn default_stop_timeout_seconds() -> u64 {
    30
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum InstanceDesiredState {
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum InstanceObservedState {
    Absent,
    Creating,
    Warming,
    Healthy,
    Degraded,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct InstanceDocument {
    pub schema: String,
    pub id: String,
    pub name: String,
    pub model_id: String,
    pub model: String,
    pub model_commit: String,
    pub engine_id: String,
    pub engine_fingerprint: String,
    pub artifacts: ModelArtifactsDocument,
    pub artifact_fingerprint: String,
    pub objective: String,
    pub resources: RecipeResourceEnvelopeDocument,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    pub generation: u64,
    pub desired: InstanceDesiredState,
    pub observed: InstanceObservedState,
    pub endpoint: Option<String>,
    pub healthy: bool,
    pub started_at: Option<String>,
    #[serde(default)]
    pub startup_milliseconds: Option<u64>,
    pub last_failure: Option<String>,
    #[serde(default)]
    pub restart_failures: u32,
    #[serde(default)]
    pub restart_suppressed: bool,
    #[serde(default)]
    pub quarantine: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct InstanceListDocument {
    pub schema: String,
    pub instances: Vec<InstanceDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct EngineLogDocument {
    pub schema: String,
    pub instance_id: String,
    pub generation: u64,
    pub cursor: u64,
    pub next_cursor: u64,
    pub truncated: bool,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DownloadPlanDocument {
    pub schema: String,
    pub repository: String,
    pub commit: String,
    #[serde(default)]
    pub artifacts: Option<ModelArtifactsDocument>,
    pub logical_bytes: u64,
    pub unique_bytes: u64,
    pub temporary_bytes: u64,
    pub disk_reserve_bytes: u64,
}

fn default_model_revision() -> String {
    "main".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelDocument {
    pub schema: String,
    pub id: String,
    pub canonical: String,
    pub repository: String,
    pub commit: String,
    pub snapshot: String,
    #[serde(default)]
    pub artifacts: Option<ModelArtifactsDocument>,
    pub logical_bytes: u64,
    pub unique_bytes: u64,
    pub aliases: Vec<String>,
    pub active_instances: Vec<String>,
    pub transport: String,
    pub verified_at: String,
    pub gated: bool,
    pub license: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ModelListDocument {
    pub schema: String,
    pub models: Vec<ModelDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RemoveModelRequest {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RemovalPlanDocument {
    pub schema: String,
    pub model_id: String,
    pub snapshot_bytes: u64,
    pub reclaimable_bytes: u64,
    pub shared_bytes: u64,
    pub active_instances: Vec<String>,
    pub aliases: Vec<String>,
    pub requires_confirmation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DoctorCheck {
    pub code: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct DoctorDocument {
    pub schema: String,
    pub checks: Vec<DoctorCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct CertificateStatusDocument {
    pub schema: String,
    pub valid: bool,
    pub dns_sans: Vec<String>,
    pub ip_sans: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct ProblemDocument {
    pub schema: String,
    pub r#type: String,
    pub code: String,
    pub status: u16,
    pub detail: String,
    pub remediation: Vec<String>,
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl OperationState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct OperationProgress {
    pub stage: String,
    pub current: Option<u64>,
    pub total: Option<u64>,
    pub unit: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct OperationDocument {
    pub schema: String,
    pub id: String,
    pub kind: String,
    pub actor_token_id: String,
    pub target: Option<String>,
    pub state: OperationState,
    pub progress: OperationProgress,
    pub created_at: String,
    pub updated_at: String,
    pub result: Option<serde_json::Value>,
    pub problem: Option<ProblemDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct OperationListDocument {
    pub schema: String,
    pub operations: Vec<OperationDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct OperationEvent {
    pub schema: String,
    pub id: u64,
    pub operation_id: String,
    pub state: OperationState,
    pub progress: OperationProgress,
    pub occurred_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub enum TokenScope {
    #[serde(rename = "models:read")]
    ModelsRead,
    #[serde(rename = "models:write")]
    ModelsWrite,
    #[serde(rename = "instances:read")]
    InstancesRead,
    #[serde(rename = "instances:write")]
    InstancesWrite,
    #[serde(rename = "inference")]
    Inference,
    #[serde(rename = "logs:read")]
    LogsRead,
    #[serde(rename = "operations:read")]
    OperationsRead,
    #[serde(rename = "operations:cancel")]
    OperationsCancel,
    #[serde(rename = "benchmarks:read")]
    BenchmarksRead,
    #[serde(rename = "benchmarks:write")]
    BenchmarksWrite,
    #[serde(rename = "admin")]
    Admin,
}

impl TokenScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ModelsRead => "models:read",
            Self::ModelsWrite => "models:write",
            Self::InstancesRead => "instances:read",
            Self::InstancesWrite => "instances:write",
            Self::Inference => "inference",
            Self::LogsRead => "logs:read",
            Self::OperationsRead => "operations:read",
            Self::OperationsCancel => "operations:cancel",
            Self::BenchmarksRead => "benchmarks:read",
            Self::BenchmarksWrite => "benchmarks:write",
            Self::Admin => "admin",
        }
    }
}

impl std::str::FromStr for TokenScope {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "models:read" => Ok(Self::ModelsRead),
            "models:write" => Ok(Self::ModelsWrite),
            "instances:read" => Ok(Self::InstancesRead),
            "instances:write" => Ok(Self::InstancesWrite),
            "inference" => Ok(Self::Inference),
            "logs:read" => Ok(Self::LogsRead),
            "operations:read" => Ok(Self::OperationsRead),
            "operations:cancel" => Ok(Self::OperationsCancel),
            "benchmarks:read" => Ok(Self::BenchmarksRead),
            "benchmarks:write" => Ok(Self::BenchmarksWrite),
            "admin" => Ok(Self::Admin),
            _ => Err(format!("unknown Spark token scope: {value}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct TokenDocument {
    pub schema: String,
    pub id: String,
    pub name: String,
    pub scopes: Vec<TokenScope>,
    pub allowed_cidrs: Vec<String>,
    pub expires_at: Option<String>,
    pub max_concurrent_inference: u32,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct TokenListDocument {
    pub schema: String,
    pub tokens: Vec<TokenDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
pub struct TokenCreatedDocument {
    pub operation: OperationDocument,
    pub token: TokenDocument,
    pub bearer_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TokenCreateRequest {
    pub name: String,
    pub scopes: Vec<TokenScope>,
    #[serde(default)]
    pub allowed_cidrs: Vec<String>,
    pub expires_at: Option<String>,
    #[serde(default = "default_inference_concurrency")]
    pub max_concurrent_inference: u32,
}

fn default_inference_concurrency() -> u32 {
    1
}

#[cfg(feature = "spark-agent")]
pub fn canonical_request_sha256<T: Serialize>(request: &T) -> Result<String, serde_json::Error> {
    let canonical = serde_json::to_vec(request)?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostInventory {
    pub schema: String,
    pub probe_sha256: String,
    pub hostname: String,
    pub dgx_software_build: String,
    pub os: OsInventory,
    pub architecture: String,
    pub kernel_release: String,
    pub nvidia_driver_version: String,
    pub cuda_runtime_version: String,
    pub firmware_identity: String,
    pub gpu: GpuInventory,
    pub docker: DockerInventory,
    pub nvidia_container_toolkit_version: String,
    pub systemd_version: String,
    pub lsm: LsmInventory,
    pub python: PythonInventory,
    pub memory: MemoryInventory,
    pub storage: Vec<StorageInventory>,
    pub lan_addresses: Vec<String>,
    pub existing_installation: ExistingInstallation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OsInventory {
    pub id: String,
    pub version_id: String,
    pub pretty_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuInventory {
    pub name: String,
    pub compute_capability: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerInventory {
    pub version: String,
    pub active: bool,
    pub login_user_socket_access: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LsmInventory {
    pub kind: String,
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonInventory {
    pub version: String,
    pub venv_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryInventory {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageInventory {
    pub mount_point: String,
    pub source: String,
    pub filesystem: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExistingInstallation {
    pub present: bool,
    pub current_release: Option<String>,
    pub state_schema: Option<String>,
}

pub fn decode_inventory(bytes: &[u8]) -> Result<HostInventory, String> {
    let inventory: HostInventory = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid bootstrap inventory: {error}"))?;
    if inventory.schema != INVENTORY_SCHEMA {
        return Err(format!(
            "unsupported bootstrap inventory schema: {}",
            inventory.schema
        ));
    }
    if !is_sha256(&inventory.probe_sha256) {
        return Err("bootstrap inventory has an invalid probe SHA-256".into());
    }
    Ok(inventory)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedVersions {
    pub dgx_software_build: String,
    pub os_id: String,
    pub os_version_id: String,
    pub architecture: String,
    pub kernel_release: String,
    pub nvidia_driver_version: String,
    pub cuda_runtime_version: String,
    pub firmware_identity: String,
    pub docker_version: String,
    pub nvidia_container_toolkit_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedFingerprint {
    pub schema: String,
    pub sha256: String,
    pub versions: ProtectedVersions,
}

impl ProtectedFingerprint {
    pub fn from_inventory(inventory: &HostInventory) -> Result<Self, String> {
        let versions = ProtectedVersions {
            dgx_software_build: inventory.dgx_software_build.clone(),
            os_id: inventory.os.id.clone(),
            os_version_id: inventory.os.version_id.clone(),
            architecture: inventory.architecture.clone(),
            kernel_release: inventory.kernel_release.clone(),
            nvidia_driver_version: inventory.nvidia_driver_version.clone(),
            cuda_runtime_version: inventory.cuda_runtime_version.clone(),
            firmware_identity: inventory.firmware_identity.clone(),
            docker_version: inventory.docker.version.clone(),
            nvidia_container_toolkit_version: inventory.nvidia_container_toolkit_version.clone(),
        };
        let canonical = serde_json::to_vec(&versions)
            .map_err(|error| format!("encode protected fingerprint: {error}"))?;
        let sha256 = format!("{:x}", Sha256::digest(canonical));
        Ok(Self {
            schema: FINGERPRINT_SCHEMA.into(),
            sha256,
            versions,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Directory,
    File,
    Symlink,
    Identity,
    Credential,
    Certificate,
    Recipe,
    SystemdUnit,
    LsmPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentIdentity {
    Sha256(String),
    GeneratedAtInstall,
    SignedReleaseManifest,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Applicability {
    ApplyNow { phase: ExecutionPhase },
    Deferred { roadmap_step: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    RemoteInstall,
    LocalCredentialStore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedAsset {
    pub kind: AssetKind,
    pub path_or_name: String,
    pub owner: String,
    pub mode: String,
    pub content: ContentIdentity,
    pub disposition: String,
    pub applicability: Applicability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceTransition {
    pub unit: String,
    pub before: String,
    pub after: String,
    pub applicability: Applicability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationPlan {
    pub database: String,
    pub from_schema: Option<String>,
    pub to_schema: String,
    pub backup_before_migration: bool,
    pub applicability: Applicability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackPlan {
    pub activation_path: String,
    pub target_release: Option<String>,
    pub retain_preceding_release: bool,
    pub restore_database_backup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectedUpdateClass {
    OperatingSystem,
    Kernel,
    NvidiaDriver,
    CudaRuntime,
    Firmware,
    Bootloader,
    Docker,
    NvidiaContainerToolkit,
    SystemPython,
    Sysctl,
    Swap,
    Clocks,
    Power,
    TransparentHugePages,
    Firewall,
    EngineImagePull,
    ModelDownload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeEvidence {
    pub local_sha256: String,
    pub reported_sha256: String,
    pub remote_path: String,
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InstallExecution {
    Planned,
    Applied {
        changed: bool,
        active_release: String,
        preceding_release: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallManifest {
    pub schema: String,
    pub operation: String,
    pub dry_run: bool,
    pub installation_performed: bool,
    pub host_alias: String,
    pub listen_address: String,
    pub listen_port: u16,
    pub probe: ProbeEvidence,
    pub inventory: HostInventory,
    pub protected_before: ProtectedFingerprint,
    pub protected_versions_must_remain_unchanged: bool,
    pub rejected_updates: Vec<RejectedUpdateClass>,
    pub assets: Vec<PlannedAsset>,
    pub service_transitions: Vec<ServiceTransition>,
    pub migration: MigrationPlan,
    pub rollback: RollbackPlan,
    pub approval_sha256: String,
    pub execution: InstallExecution,
}

#[cfg(test)]
mod tests {
    use super::{
        decode_inventory, ModelArtifactFileDocument, ModelArtifactFormat,
        ModelArtifactSelectorDocument, ModelArtifactsDocument, ModelAuxiliaryArtifactDocument,
    };

    const INVENTORY: &str = r#"{
        "schema":"sy.spark.bootstrap.inventory/v1",
        "probe_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "hostname":"spark",
        "dgx_software_build":"7.5.0",
        "os":{"id":"ubuntu","version_id":"24.04","pretty_name":"Ubuntu 24.04"},
        "architecture":"aarch64","kernel_release":"6.17.0-1022-nvidia",
        "nvidia_driver_version":"580.159.03","cuda_runtime_version":"13.0",
        "firmware_identity":"GB10:1.0","gpu":{"name":"NVIDIA GB10","compute_capability":"12.1"},
        "docker":{"version":"29.2.1","active":true,"login_user_socket_access":false},
        "nvidia_container_toolkit_version":"1.19.0","systemd_version":"255",
        "lsm":{"kind":"apparmor","mode":"enforce"},
        "python":{"version":"3.12.3","venv_available":true},
        "memory":{"total_bytes":127775277056,"available_bytes":123480309760,"swap_total_bytes":17179869184},
        "storage":[{"mount_point":"/","source":"/dev/nvme0n1p2","filesystem":"ext4","total_bytes":1000000000000,"free_bytes":793000000000}],
        "lan_addresses":["10.1.30.143"],
        "existing_installation":{"present":false,"current_release":null,"state_schema":null}
    }"#;

    #[test]
    fn inventory_rejects_unknown_or_missing_contract_fields() {
        assert!(decode_inventory(INVENTORY.as_bytes()).is_ok());
        let unknown = INVENTORY.replace(
            "\"hostname\":\"spark\"",
            "\"hostname\":\"spark\",\"extra\":true",
        );
        assert!(decode_inventory(unknown.as_bytes()).is_err());
        let missing = INVENTORY.replace("\"architecture\":\"aarch64\",", "");
        assert!(decode_inventory(missing.as_bytes()).is_err());
    }

    #[test]
    fn artifact_format_represents_gguf_and_safetensors() {
        assert_eq!(
            serde_json::to_string(&ModelArtifactFormat::Gguf).unwrap(),
            "\"gguf\""
        );
        assert_eq!(
            serde_json::from_str::<ModelArtifactFormat>("\"safetensors\"").unwrap(),
            ModelArtifactFormat::Safetensors
        );
    }

    #[test]
    fn artifact_file_rejects_unknown_fields() {
        let value = r#"{"path":"model.gguf","bytes":7,"sha256":null,"extra":true}"#;
        assert!(serde_json::from_str::<ModelArtifactFileDocument>(value).is_err());
    }

    #[test]
    fn model_artifacts_reject_unknown_catalog_fields() {
        const JSON: &str = r#"{"schema":"sy.spark.model-artifacts/v2","format":"gguf","primary":{"path":"model.gguf","bytes":7,"sha256":null},"auxiliary":[],"quantization":null,"capabilities":[],"configured_alias":null,"engine":"hidden"}"#;
        assert!(serde_json::from_str::<ModelArtifactsDocument>(JSON).is_err());
    }

    #[test]
    fn model_artifacts_round_trip_without_losing_exact_files() {
        const JSON: &str = r#"{"schema":"sy.spark.model-artifacts/v2","format":"gguf","primary":{"path":"model.gguf","bytes":7,"sha256":"aaa"},"auxiliary":[{"role":"projector","path":"mmproj.gguf","bytes":3,"sha256":"bbb"}],"quantization":"Q4_K_XL","capabilities":["text","vision"],"configured_alias":"qwen","engine_profile":"coding"}"#;
        let artifacts: ModelArtifactsDocument = serde_json::from_str(JSON).unwrap();
        assert_eq!(serde_json::to_string(&artifacts).unwrap(), JSON);
    }

    #[test]
    fn auxiliary_roles_round_trip_and_unlabelled_files_fail_closed() {
        let projector = r#"{"role":"projector","path":"mmproj.gguf","bytes":3,"sha256":null}"#;
        let shard = r#"{"role":"weight_shard","path":"model-02.gguf","bytes":5,"sha256":null}"#;
        let draft = r#"{"role":"draft_model","path":"mtp.gguf","bytes":7,"sha256":null}"#;
        let future = r#"{"role":"future_adapter","path":"adapter.gguf","bytes":11,"sha256":null}"#;
        assert!(serde_json::from_str::<ModelAuxiliaryArtifactDocument>(projector).is_ok());
        assert!(serde_json::from_str::<ModelAuxiliaryArtifactDocument>(shard).is_ok());
        assert!(serde_json::from_str::<ModelAuxiliaryArtifactDocument>(draft).is_ok());
        assert!(serde_json::from_str::<ModelAuxiliaryArtifactDocument>(future).is_ok());
        assert!(serde_json::from_str::<ModelAuxiliaryArtifactDocument>(
            r#"{"path":"model-02.gguf","bytes":5,"sha256":null}"#
        )
        .is_err());
    }

    #[test]
    fn model_document_without_artifacts_remains_readable_for_reindexing() {
        const JSON: &str = r#"{"schema":"sy.spark.model/v1","id":"m_1","canonical":"huggingface:o/m@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","repository":"o/m","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","snapshot":"models--o--m/snapshots/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","logical_bytes":1,"unique_bytes":1,"aliases":[],"active_instances":[],"transport":"fixture","verified_at":"2026-08-24T00:00:00Z","gated":false,"license":null}"#;
        assert_eq!(
            serde_json::from_str::<super::ModelDocument>(JSON)
                .unwrap()
                .artifacts,
            None
        );
        let explicit = JSON.replacen(
            "\"logical_bytes\"",
            "\"artifacts\":null,\"logical_bytes\"",
            1,
        );
        assert_eq!(
            serde_json::from_str::<super::ModelDocument>(&explicit)
                .unwrap()
                .artifacts,
            None
        );
    }

    #[test]
    fn download_plan_exposes_exact_artifacts() {
        const JSON: &str = r#"{"schema":"plan/v1","repository":"o/m","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","artifacts":{"schema":"sy.spark.model-artifacts/v2","format":"safetensors","primary":{"path":"model.safetensors","bytes":7,"sha256":null},"auxiliary":[],"quantization":null,"capabilities":["text"],"configured_alias":null},"logical_bytes":7,"unique_bytes":7,"temporary_bytes":7,"disk_reserve_bytes":8}"#;
        let plan: super::DownloadPlanDocument = serde_json::from_str(JSON).unwrap();
        assert_eq!(plan.artifacts.unwrap().primary.path, "model.safetensors");
    }

    #[test]
    fn download_request_round_trips_exact_artifact_selectors() {
        const JSON: &str = r#"{"repository":"owner/model","revision":"main","alias":null,"artifact":"weights/model.gguf","auxiliary":[{"role":"projector","path":"vision/mmproj.gguf"}],"update_alias":false,"dry_run":true}"#;
        let request: super::DownloadRequest = serde_json::from_str(JSON).unwrap();
        assert_eq!(serde_json::to_string(&request).unwrap(), JSON);
    }

    #[test]
    fn explicit_auxiliary_selector_requires_a_role() {
        assert!("projector=vision/mmproj.gguf"
            .parse::<ModelArtifactSelectorDocument>()
            .is_ok());
        assert!("weight_shard=model-00002.gguf"
            .parse::<ModelArtifactSelectorDocument>()
            .is_ok());
        assert!("vision/mmproj.gguf"
            .parse::<ModelArtifactSelectorDocument>()
            .is_err());
    }
}
