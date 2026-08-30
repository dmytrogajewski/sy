//! Optional declarative aliases for recommended Spark model artifacts.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::model::{Alias, CommitSha, ExpectedFile, Repository};
use super::wire::{
    ModelArtifactFileDocument, ModelArtifactFormat, ModelArtifactRole, ModelArtifactsDocument,
    ModelAuxiliaryArtifactDocument,
};

pub const MODEL_CATALOG_SCHEMA: &str = "sy.spark.models/v2";
const MODEL_ARTIFACTS_SCHEMA: &str = "sy.spark.model-artifacts/v2";
const MAX_AUXILIARY_ARTIFACTS: usize = 511;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogConfig {
    schema: String,
    models: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogEntry {
    aliases: Vec<String>,
    repository: String,
    revision: String,
    artifact: CatalogArtifact,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogArtifact {
    format: ModelArtifactFormat,
    primary: ModelArtifactFileDocument,
    #[serde(default)]
    auxiliary: Vec<ModelAuxiliaryArtifactDocument>,
    quantization: Option<String>,
    capabilities: Vec<String>,
    #[serde(default)]
    engine_profile: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CatalogModel {
    repository: String,
    revision: String,
    artifacts: ModelArtifactsDocument,
}

#[derive(Debug, Clone)]
pub struct ModelCatalog {
    aliases: BTreeMap<String, CatalogModel>,
}

impl CatalogModel {
    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn artifacts(&self) -> &ModelArtifactsDocument {
        &self.artifacts
    }
}

impl ModelCatalog {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("read Spark model catalog: {error}"))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let config: CatalogConfig =
            toml::from_str(text).map_err(|error| format!("parse Spark model catalog: {error}"))?;
        validate_config(&config)?;
        let mut aliases = BTreeMap::new();
        for entry in config.models {
            for alias in &entry.aliases {
                aliases.insert(alias.clone(), resolved(&entry, alias));
            }
        }
        let catalog = Self { aliases };
        catalog.validate_resolutions()?;
        Ok(catalog)
    }

    pub fn resolve(&self, alias: &str) -> Option<&CatalogModel> {
        self.aliases.get(alias)
    }

    fn validate_resolutions(&self) -> Result<(), String> {
        for alias in self.aliases.keys() {
            let model = self
                .resolve(alias)
                .ok_or_else(|| format!("model alias {alias} cannot be resolved"))?;
            if model.repository().is_empty()
                || model.revision().is_empty()
                || model.artifacts().configured_alias.as_deref() != Some(alias)
            {
                return Err(format!("model alias {alias} resolved inconsistently"));
            }
        }
        Ok(())
    }
}

fn resolved(entry: &CatalogEntry, alias: &str) -> CatalogModel {
    CatalogModel {
        repository: entry.repository.clone(),
        revision: entry.revision.clone(),
        artifacts: ModelArtifactsDocument {
            schema: MODEL_ARTIFACTS_SCHEMA.into(),
            format: entry.artifact.format,
            primary: entry.artifact.primary.clone(),
            auxiliary: entry.artifact.auxiliary.clone(),
            quantization: entry.artifact.quantization.clone(),
            capabilities: entry.artifact.capabilities.clone(),
            configured_alias: Some(alias.into()),
            engine_profile: entry.artifact.engine_profile.clone(),
        },
    }
}

fn validate_config(config: &CatalogConfig) -> Result<(), String> {
    if config.schema != MODEL_CATALOG_SCHEMA || config.models.is_empty() || config.models.len() > 64
    {
        return Err("unsupported or empty Spark model catalog".into());
    }
    let mut aliases = std::collections::BTreeSet::new();
    for entry in &config.models {
        validate_entry(entry)?;
        for alias in &entry.aliases {
            Alias::parse(alias).map_err(|error| error.to_string())?;
            if !aliases.insert(alias) {
                return Err(format!("duplicate model alias {alias}"));
            }
        }
    }
    Ok(())
}

fn validate_entry(entry: &CatalogEntry) -> Result<(), String> {
    if entry.aliases.is_empty() || entry.aliases.len() > 16 {
        return Err("catalog model has no aliases".into());
    }
    Repository::parse(&entry.repository).map_err(|error| error.to_string())?;
    CommitSha::parse(&entry.revision).map_err(|error| error.to_string())?;
    validate_artifact(&entry.artifact)
}

fn validate_artifact(artifact: &CatalogArtifact) -> Result<(), String> {
    if artifact.auxiliary.len() > MAX_AUXILIARY_ARTIFACTS {
        return Err("model artifact set is too large".into());
    }
    if artifact.engine_profile.as_deref().is_some_and(|profile| {
        profile.is_empty()
            || profile.len() > 128
            || !profile
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    }) {
        return Err("model engine profile is invalid".into());
    }
    let mut paths = std::collections::BTreeSet::new();
    validate_file(
        &artifact.primary.path,
        artifact.primary.bytes,
        artifact.primary.sha256.clone(),
    )?;
    paths.insert(&artifact.primary.path);
    for file in &artifact.auxiliary {
        validate_file(&file.path, file.bytes, file.sha256.clone())?;
        if !paths.insert(&file.path) {
            return Err(format!("duplicate model artifact path {}", file.path));
        }
    }
    validate_artifact_traits(artifact)
}

fn validate_file(path: &str, bytes: u64, sha256: Option<String>) -> Result<(), String> {
    if bytes == 0 || path.len() > 512 {
        return Err("model artifact size must be positive".into());
    }
    ExpectedFile::new(path, bytes, sha256)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn validate_artifact_traits(artifact: &CatalogArtifact) -> Result<(), String> {
    const CAPABILITIES: [&str; 5] = [
        "reasoning",
        "text_embeddings",
        "text_generation",
        "tool_calling",
        "vision",
    ];
    let capabilities = artifact
        .capabilities
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    if capabilities.is_empty()
        || capabilities.len() != artifact.capabilities.len()
        || capabilities
            .iter()
            .any(|value| !CAPABILITIES.contains(&value.as_str()))
    {
        return Err("model artifact capabilities are invalid".into());
    }
    let has = |capability: &str| {
        artifact
            .capabilities
            .iter()
            .any(|value| value == capability)
    };
    if (has("reasoning") || has("tool_calling")) && !has("text_generation")
        || has("vision") && artifact.auxiliary.is_empty()
    {
        return Err("model artifact capability dependencies are invalid".into());
    }
    let quantization = artifact.quantization.as_deref().unwrap_or_default();
    if quantization.is_empty()
        || quantization.len() > 32
        || !quantization
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("model artifact quantization is invalid".into());
    }
    let primary_matches = match artifact.format {
        ModelArtifactFormat::Gguf => artifact.primary.path.ends_with(".gguf"),
        ModelArtifactFormat::Safetensors => {
            artifact.primary.path.ends_with(".safetensors")
                || artifact.primary.path.ends_with(".safetensors.index.json")
        }
    };
    let auxiliary_matches = match artifact.format {
        ModelArtifactFormat::Gguf => artifact
            .auxiliary
            .iter()
            .all(|file| file.path.ends_with(".gguf")),
        ModelArtifactFormat::Safetensors => artifact.auxiliary.iter().all(|file| {
            file.role == ModelArtifactRole::WeightShard && file.path.ends_with(".safetensors")
        }),
    };
    if !primary_matches || !auxiliary_matches {
        return Err("model artifact files disagree with their format".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ModelCatalog;
    use crate::spark::wire::{ModelArtifactFormat, ModelArtifactRole};

    const RECOMMENDED: &str = include_str!("../../configs/sy/spark/models.toml");

    #[test]
    fn recommended_catalog_resolves_exact_immutable_artifacts() {
        let catalog = ModelCatalog::parse(RECOMMENDED).unwrap();
        let expected = [
            (
                "ornith-1.5:9b-q4-k-m",
                "ornith-ai/Ornith-1.5-9B-GGUF",
                "abdd624b12ebf020b767fff532ff44fe552b28c3",
                "Ornith-1.5-9B-Q4_K_M.gguf",
                1,
            ),
            (
                "qwen3.8:27b",
                "ggml-org/Qwen3.8-27B-GGUF",
                "0669b98607d47046c7c2b3f801011d54a08cfccf",
                "Qwen3.8-27B-Q4_K_M.gguf",
                2,
            ),
            (
                "ornith-1.5:35b",
                "ornith-ai/Ornith-1.5-35B-A3B-GGUF",
                "12393612fd4f730ff5aadc23e9b8f9648aa49ceb",
                "Ornith-1.5-35B-Q4_K_M.gguf",
                1,
            ),
            (
                "muse-glimmer:30b",
                "lactroiii/Muse-Glimmer-30B-GGUF",
                "c8e212a87fbc137e44463663fb7550ae92079849",
                "Muse-Glimmer-30B-KQuant-Dynamic-Q4_K_XL.gguf",
                1,
            ),
            (
                "qwen3.8:27b-fp8",
                "Qwen/Qwen3.8-27B-FP8",
                "017b9c7af6b5689d5dd426a76e0bc077eb5ca20a",
                "model.safetensors.index.json",
                65,
            ),
            (
                "ornith-1.5:35b-fp8",
                "ornith-ai/Ornith-1.5-35B-A3B-FP8",
                "fab11c26e2325a42f4b32da0249c819a0bade1b1",
                "model.safetensors.index.json",
                16,
            ),
            (
                "muse-glimmer:30b-fp8",
                "RedHatAI/Muse-Glimmer-30B-FP8-block",
                "8ed2e29141d4fef439b9a0e15e0a2678bc190a82",
                "model.safetensors.index.json",
                2,
            ),
            (
                "qwen3.8:flash-next-nvfp4",
                "RadixArk/Qwen3.8-Flash-Next-NVFP4",
                "7b719225242aacd3dbd3f9407468c2ee9a9d2594",
                "model.safetensors.index.json",
                206,
            ),
            (
                "qwen3.6:35b-a3b-nvfp4",
                "nvidia/Qwen3.6-35B-A3B-NVFP4",
                "491c2f1ea524c639598bf8fa787a93fed5a6fbce",
                "model.safetensors.index.json",
                3,
            ),
        ];
        for (alias, repository, revision, primary, auxiliary) in expected {
            let model = catalog.resolve(alias).unwrap();
            assert_eq!(
                (
                    model.repository(),
                    model.revision(),
                    model.artifacts().primary.path.as_str()
                ),
                (repository, revision, primary)
            );
            assert_eq!(model.artifacts().auxiliary.len(), auxiliary);
            assert_eq!(model.artifacts().configured_alias.as_deref(), Some(alias));
        }
    }

    #[test]
    fn recommended_catalog_labels_projectors_and_weight_shards_explicitly() {
        let catalog = ModelCatalog::parse(RECOMMENDED).unwrap();
        for alias in ["ornith-1.5:9b-q4-k-m", "ornith-1.5:35b", "muse-glimmer:30b"] {
            assert!(catalog
                .resolve(alias)
                .unwrap()
                .artifacts()
                .auxiliary
                .iter()
                .all(|file| file.role == ModelArtifactRole::Projector));
        }
        assert_eq!(
            catalog
                .resolve("qwen3.8:27b")
                .unwrap()
                .artifacts()
                .auxiliary
                .iter()
                .map(|file| file.role.as_str())
                .collect::<Vec<_>>(),
            ["projector", "draft_model"]
        );
        for alias in [
            "qwen3.8:27b-fp8",
            "ornith-1.5:35b-fp8",
            "muse-glimmer:30b-fp8",
            "qwen3.8:flash-next-nvfp4",
        ] {
            let artifacts = catalog.resolve(alias).unwrap().artifacts();
            assert_eq!(artifacts.format, ModelArtifactFormat::Safetensors);
            assert!(artifacts
                .auxiliary
                .iter()
                .all(|file| file.role == ModelArtifactRole::WeightShard));
        }
    }

    #[test]
    fn recommended_ornith_uses_its_declarative_engine_profile() {
        let catalog = ModelCatalog::parse(RECOMMENDED).unwrap();
        assert_eq!(
            catalog
                .resolve("ornith-1.5:9b-q4-k-m")
                .unwrap()
                .artifacts()
                .engine_profile
                .as_deref(),
            Some("ornith-coding")
        );
    }

    #[test]
    fn recommended_qwen_pins_target_draft_and_mtp_profile() {
        let catalog = ModelCatalog::parse(RECOMMENDED).unwrap();
        let artifacts = catalog.resolve("qwen3.8:27b").unwrap().artifacts();
        assert_eq!(
            (
                artifacts.quantization.as_deref(),
                artifacts.engine_profile.as_deref(),
                artifacts.primary.path.as_str(),
                artifacts.primary.bytes,
                artifacts.primary.sha256.as_deref(),
            ),
            (
                Some("Q4_K_M"),
                Some("qwen3.8-mtp"),
                "Qwen3.8-27B-Q4_K_M.gguf",
                18_973_870_432,
                Some("31629f53165ab6a7dad8c9847dcfd1fdf55829dac1e6e748f4a68581b0033d34"),
            )
        );
        assert_eq!(
            artifacts
                .auxiliary
                .iter()
                .map(|file| (file.role.as_str(), file.path.as_str()))
                .collect::<Vec<_>>(),
            [
                ("projector", "mmproj-Qwen3.8-27B-BF16.gguf"),
                ("draft_model", "mtp-Qwen3.8-27B-Q4_0.gguf"),
            ]
        );
    }

    #[test]
    fn duplicate_alias_or_mutable_revision_is_rejected() {
        let duplicate = RECOMMENDED.replacen("ornith-1.5:35b", "qwen3.8:27b", 1);
        assert!(ModelCatalog::parse(&duplicate).is_err());
        let mutable = RECOMMENDED.replacen("0669b98607d47046c7c2b3f801011d54a08cfccf", "main", 1);
        assert!(ModelCatalog::parse(&mutable).is_err());
    }

    #[test]
    fn model_names_do_not_affect_resolution() {
        let catalog = ModelCatalog::parse(RECOMMENDED).unwrap();
        assert!(catalog.resolve("ggml-org/Qwen3.8-27B-GGUF").is_none());
        assert!(catalog.resolve("Qwen3.8:27b").is_none());
        assert!(catalog.resolve("qwen3.8:27b").is_some());
    }

    #[test]
    fn unknown_fields_and_path_traversal_are_rejected() {
        let unknown = RECOMMENDED.replacen(
            "format = \"gguf\"",
            "format = \"gguf\"\nengine = \"hidden\"",
            1,
        );
        assert!(ModelCatalog::parse(&unknown).is_err());
        let traversal = RECOMMENDED.replacen("Qwen3.8-27B-Q4_K_M.gguf", "../model.gguf", 1);
        assert!(ModelCatalog::parse(&traversal).is_err());
        let unlabelled = RECOMMENDED.replacen("role = \"projector\"\n", "", 1);
        assert!(ModelCatalog::parse(&unlabelled).is_err());
        let invalid_profile = RECOMMENDED.replacen("ornith-coding", "../embedded", 1);
        assert!(ModelCatalog::parse(&invalid_profile).is_err());
    }

    #[test]
    fn artifact_sizes_and_capability_dependencies_are_validated() {
        let zero = RECOMMENDED.replacen("bytes = 18973870432", "bytes = 0", 1);
        assert!(ModelCatalog::parse(&zero).is_err());
        let tools_without_text = RECOMMENDED.replacen(
            "\"reasoning\", \"text_generation\", \"tool_calling\", \"vision\"",
            "\"tool_calling\", \"vision\"",
            1,
        );
        assert!(ModelCatalog::parse(&tools_without_text).is_err());
    }
}
