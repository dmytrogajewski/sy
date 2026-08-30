//! Immutable model acquisition and native Hugging Face cache verification.

use std::{
    collections::BTreeSet,
    fmt, fs,
    io::{BufReader, Read},
    path::{Component, Path, PathBuf},
};

use futures_util::StreamExt;
use hf_hub::repository::RepoTreeEntry;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};

use super::wire::{
    ModelArtifactFileDocument, ModelArtifactFormat, ModelArtifactRole,
    ModelArtifactSelectorDocument, ModelArtifactsDocument, ModelAuxiliaryArtifactDocument,
    ModelDocument, MODEL_SCHEMA,
};

const MODEL_PROGRESS_QUEUE_CAPACITY: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Repository(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Revision(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommitSha(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Alias(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInputError(&'static str);

impl fmt::Display for ModelInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ModelInputError {}

impl Repository {
    pub fn parse(value: &str) -> Result<Self, ModelInputError> {
        let mut parts = value.split('/');
        let owner = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        if parts.next().is_some() || !valid_component(owner, false) || !valid_component(name, false)
        {
            return Err(ModelInputError("repository must be one safe owner/name"));
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Revision {
    pub fn parse(value: &str) -> Result<Self, ModelInputError> {
        if value.is_empty()
            || value.len() > 200
            || value.split('/').any(|part| !valid_component(part, true))
        {
            return Err(ModelInputError(
                "revision contains an unsafe path component",
            ));
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl CommitSha {
    pub fn parse(value: &str) -> Result<Self, ModelInputError> {
        if value.len() != 40
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ModelInputError(
                "commit must be forty lowercase hexadecimal bytes",
            ));
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Alias {
    pub fn parse(value: &str) -> Result<Self, ModelInputError> {
        let Some((name, tag)) = value.split_once(':') else {
            return Err(ModelInputError("alias must use name:tag form"));
        };
        if value.matches(':').count() != 1 || !valid_alias_part(name) || !valid_alias_part(tag) {
            return Err(ModelInputError(
                "alias must use lowercase safe name:tag form",
            ));
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_component(value: &str, allow_ref_chars: bool) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-')
                || (allow_ref_chars && byte == b'@')
        })
}

fn valid_alias_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedFile {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
}

impl ExpectedFile {
    pub fn new(path: &str, size: u64, sha256: Option<String>) -> Result<Self, ModelInputError> {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || sha256.as_ref().is_some_and(|hash| {
                hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
        {
            return Err(ModelInputError("expected file descriptor is unsafe"));
        }
        Ok(Self { path, size, sha256 })
    }
}

#[derive(Debug)]
pub struct VerifiedSnapshot {
    pub path: PathBuf,
    pub logical_bytes: u64,
}

pub fn verify_snapshot(
    cache: &Path,
    repository: &Repository,
    commit: &CommitSha,
    expected: &[ExpectedFile],
) -> Result<VerifiedSnapshot, ModelInputError> {
    let repo = cache.join(repository_cache_name(repository));
    let canonical_repo = repo
        .canonicalize()
        .map_err(|_| ModelInputError("repository cache is missing"))?;
    for entry in walkdir::WalkDir::new(repo.join("blobs")).follow_links(false) {
        let entry =
            entry.map_err(|_| ModelInputError("repository blob tree cannot be inspected"))?;
        if entry.file_name().to_string_lossy().ends_with(".incomplete") {
            return Err(ModelInputError(
                "repository cache contains an incomplete blob",
            ));
        }
    }
    let snapshot = repo.join("snapshots").join(commit.as_str());
    let canonical_snapshot = snapshot
        .canonicalize()
        .map_err(|_| ModelInputError("snapshot is missing"))?;
    if !canonical_snapshot.starts_with(&canonical_repo) {
        return Err(ModelInputError("snapshot escaped its repository cache"));
    }
    let mut logical_bytes = 0_u64;
    for expected_file in expected {
        let pointer = snapshot.join(&expected_file.path);
        let metadata = fs::symlink_metadata(&pointer)
            .map_err(|_| ModelInputError("snapshot file is missing"))?;
        if !metadata.file_type().is_symlink() {
            return Err(ModelInputError(
                "snapshot file is not a native cache symlink",
            ));
        }
        let target = pointer
            .canonicalize()
            .map_err(|_| ModelInputError("snapshot symlink is broken"))?;
        if !target.starts_with(&canonical_repo) {
            return Err(ModelInputError(
                "snapshot symlink escaped its repository cache",
            ));
        }
        let size = target
            .metadata()
            .map_err(|_| ModelInputError("snapshot blob metadata is unavailable"))?
            .len();
        if size != expected_file.size {
            return Err(ModelInputError(
                "snapshot blob size differs from immutable tree",
            ));
        }
        if let Some(expected_hash) = &expected_file.sha256 {
            if file_sha256(&target)? != *expected_hash {
                return Err(ModelInputError(
                    "snapshot blob hash differs from immutable tree",
                ));
            }
        }
        logical_bytes = logical_bytes
            .checked_add(size)
            .ok_or(ModelInputError("snapshot byte count overflow"))?;
    }
    Ok(VerifiedSnapshot {
        path: snapshot,
        logical_bytes,
    })
}

fn repository_cache_name(repository: &Repository) -> String {
    format!("models--{}", repository.as_str().replace('/', "--"))
}

fn snapshot_file_is_complete(
    cache: &Path,
    repository: &Repository,
    commit: &CommitSha,
    path: &str,
    size: u64,
) -> bool {
    let repo = cache.join(repository_cache_name(repository));
    let Ok(blobs) = repo.join("blobs").canonicalize() else {
        return false;
    };
    let pointer = repo.join("snapshots").join(commit.as_str()).join(path);
    if !fs::symlink_metadata(&pointer).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return false;
    }
    pointer
        .canonicalize()
        .ok()
        .filter(|target| target.starts_with(&blobs))
        .and_then(|target| target.metadata().ok())
        .is_some_and(|metadata| metadata.len() == size)
}

fn repository_blob_bytes(cache: &Path, repository: &Repository) -> Result<u64, ModelInputError> {
    walkdir::WalkDir::new(cache.join(repository_cache_name(repository)).join("blobs"))
        .follow_links(false)
        .into_iter()
        .try_fold(0_u64, |sum, entry| {
            let entry =
                entry.map_err(|_| ModelInputError("repository blobs cannot be inspected"))?;
            let bytes = if entry.file_type().is_file() {
                entry
                    .metadata()
                    .map_err(|_| ModelInputError("repository blob metadata is unavailable"))?
                    .len()
            } else {
                0
            };
            sum.checked_add(bytes)
                .ok_or(ModelInputError("repository blob byte count overflow"))
        })
}

fn file_sha256(path: &Path) -> Result<String, ModelInputError> {
    let file =
        fs::File::open(path).map_err(|_| ModelInputError("snapshot blob cannot be opened"))?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| ModelInputError("snapshot blob cannot be read"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFailure {
    XetTransport,
    XetIntegrity,
    NoProgress,
    Authentication,
    NotFound,
    Policy,
    DiskReserve,
    Cancelled,
    Other,
}

pub fn should_run_fallback(failure: TransferFailure, completed_attempts: u8) -> bool {
    completed_attempts == 0
        && matches!(
            failure,
            TransferFailure::XetTransport
                | TransferFailure::XetIntegrity
                | TransferFailure::NoProgress
        )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalPlan {
    pub snapshot: PathBuf,
    pub removable_blobs: Vec<PathBuf>,
    pub reclaimable_bytes: u64,
}

pub fn plan_removal(
    cache: &Path,
    repository: &Repository,
    commit: &CommitSha,
    active: bool,
) -> Result<RemovalPlan, ModelInputError> {
    if active {
        return Err(ModelInputError("active snapshots cannot be removed"));
    }
    let repo = cache.join(repository_cache_name(repository));
    let snapshot = repo.join("snapshots").join(commit.as_str());
    let target_blobs = snapshot_blobs(&snapshot)?;
    let mut retained_blobs = BTreeSet::new();
    for entry in fs::read_dir(repo.join("snapshots"))
        .map_err(|_| ModelInputError("snapshot inventory is unavailable"))?
    {
        let path = entry
            .map_err(|_| ModelInputError("snapshot inventory is unavailable"))?
            .path();
        if path != snapshot {
            retained_blobs.extend(snapshot_blobs(&path)?);
        }
    }
    let removable_blobs = target_blobs
        .difference(&retained_blobs)
        .cloned()
        .collect::<Vec<_>>();
    let reclaimable_bytes = removable_blobs.iter().try_fold(0_u64, |sum, path| {
        let size = path
            .metadata()
            .map_err(|_| ModelInputError("blob metadata is unavailable"))?
            .len();
        sum.checked_add(size)
            .ok_or(ModelInputError("removal byte count overflow"))
    })?;
    Ok(RemovalPlan {
        snapshot,
        removable_blobs,
        reclaimable_bytes,
    })
}

fn snapshot_blobs(snapshot: &Path) -> Result<BTreeSet<PathBuf>, ModelInputError> {
    let mut blobs = BTreeSet::new();
    for entry in walkdir::WalkDir::new(snapshot).follow_links(false) {
        let entry = entry.map_err(|_| ModelInputError("snapshot cannot be inspected"))?;
        if entry.file_type().is_symlink() {
            blobs.insert(
                entry
                    .path()
                    .canonicalize()
                    .map_err(|_| ModelInputError("snapshot symlink is broken"))?,
            );
        }
    }
    Ok(blobs)
}

#[derive(Debug, Clone)]
pub struct AcquisitionPlan {
    pub repository: Repository,
    pub commit: CommitSha,
    pub expected: Vec<ExpectedFile>,
    pub logical_bytes: u64,
    pub unique_bytes: u64,
    pub temporary_bytes: u64,
    pub gated: bool,
    pub license: Option<String>,
    pub artifacts: ModelArtifactsDocument,
}

#[derive(Debug, Clone)]
pub struct ArtifactSelection {
    primary: String,
    auxiliary: Vec<ModelArtifactSelectorDocument>,
    configured: Option<ModelArtifactsDocument>,
    automatic: bool,
}

impl ArtifactSelection {
    pub fn configured_artifacts(&self) -> Option<&ModelArtifactsDocument> {
        self.configured.as_ref()
    }

    pub fn generic<I>(primary: &str, auxiliary: I) -> Result<Self, ModelInputError>
    where
        I: IntoIterator<Item = ModelArtifactSelectorDocument>,
    {
        let auxiliary = auxiliary.into_iter().collect::<Vec<_>>();
        validate_selected_paths(primary, &auxiliary)?;
        Ok(Self {
            primary: primary.into(),
            auxiliary,
            configured: None,
            automatic: false,
        })
    }

    pub fn automatic() -> Self {
        Self {
            primary: String::new(),
            auxiliary: Vec::new(),
            configured: None,
            automatic: true,
        }
    }

    pub fn configured(artifacts: ModelArtifactsDocument) -> Result<Self, ModelInputError> {
        if artifacts.schema != "sy.spark.model-artifacts/v2" {
            return Err(ModelInputError("model artifact schema is unsupported"));
        }
        let auxiliary = artifacts
            .auxiliary
            .iter()
            .map(|file| ModelArtifactSelectorDocument {
                role: file.role.clone(),
                path: file.path.clone(),
            })
            .collect::<Vec<_>>();
        validate_selected_paths(&artifacts.primary.path, &auxiliary)?;
        Ok(Self {
            primary: artifacts.primary.path.clone(),
            auxiliary,
            configured: Some(artifacts),
            automatic: false,
        })
    }

    fn format(&self) -> Result<ModelArtifactFormat, ModelInputError> {
        let format = if self.primary.ends_with(".gguf") {
            ModelArtifactFormat::Gguf
        } else if self.primary.ends_with(".safetensors")
            || self.primary.ends_with(".safetensors.index.json")
        {
            ModelArtifactFormat::Safetensors
        } else {
            return Err(ModelInputError("primary artifact format is unsupported"));
        };
        let auxiliary_matches = self.auxiliary.iter().all(|file| match format {
            ModelArtifactFormat::Gguf => file.path.ends_with(".gguf"),
            ModelArtifactFormat::Safetensors => file.path.ends_with(".safetensors"),
        });
        if !auxiliary_matches {
            return Err(ModelInputError("auxiliary artifact format is ambiguous"));
        }
        Ok(format)
    }
}

fn infer_tree_selection(files: &[TreeFile]) -> Result<ArtifactSelection, ModelInputError> {
    let mut weights = files
        .iter()
        .filter(|file| file.path.ends_with(".safetensors"))
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    weights.sort_unstable();
    let indexes = files
        .iter()
        .filter(|file| file.path.ends_with(".safetensors.index.json"))
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    if indexes.len() == 1 && !weights.is_empty() {
        return ArtifactSelection::generic(
            indexes[0],
            weights
                .into_iter()
                .map(|path| ModelArtifactSelectorDocument {
                    role: ModelArtifactRole::WeightShard,
                    path: path.into(),
                }),
        );
    }
    if weights.len() == 1 {
        return ArtifactSelection::generic(weights[0], Vec::new());
    }
    let mut gguf = files
        .iter()
        .filter(|file| {
            file.path.ends_with(".gguf") && !file.path.to_ascii_lowercase().contains("mmproj")
        })
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    gguf.sort_unstable();
    for preferred in ["Q4_K_XL", "Q4_K_M"] {
        let matches = gguf
            .iter()
            .filter(|path| path.contains(preferred))
            .copied()
            .collect::<Vec<_>>();
        if let [selected] = matches.as_slice() {
            return ArtifactSelection::generic(selected, Vec::new());
        }
    }
    if let [selected] = gguf.as_slice() {
        return ArtifactSelection::generic(selected, Vec::new());
    }
    Err(ModelInputError(
        "model artifacts require an explicit selection",
    ))
}

fn pipeline_capabilities(pipeline: Option<&str>) -> Vec<String> {
    match pipeline {
        Some("feature-extraction" | "sentence-similarity") => vec!["text_embeddings".into()],
        _ => vec!["text_generation".into()],
    }
}

fn validate_selected_paths(
    primary: &str,
    auxiliary: &[ModelArtifactSelectorDocument],
) -> Result<(), ModelInputError> {
    let mut paths = BTreeSet::new();
    for path in std::iter::once(primary).chain(auxiliary.iter().map(|file| file.path.as_str())) {
        ExpectedFile::new(path, 1, None)?;
        if path
            .bytes()
            .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}'))
            || !paths.insert(path)
        {
            return Err(ModelInputError(
                "artifact selectors are unsafe or ambiguous",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct TreeFile {
    path: String,
    size: u64,
    blob_key: String,
    sha256: Option<String>,
}

impl TreeFile {
    #[cfg(test)]
    fn fixture((path, size): (&str, u64)) -> Self {
        Self {
            path: path.into(),
            size,
            blob_key: path.into(),
            sha256: None,
        }
    }

    fn expected(&self) -> Result<ExpectedFile, ModelInputError> {
        ExpectedFile::new(&self.path, self.size, self.sha256.clone())
    }
}

const SAFETENSORS_METADATA: &[&str] = &[
    "added_tokens.json",
    "chat_template.jinja",
    "config.json",
    "generation_config.json",
    "merges.txt",
    "preprocessor_config.json",
    "processor_config.json",
    "special_tokens_map.json",
    "tokenizer.json",
    "tokenizer.model",
    "tokenizer_config.json",
    "video_preprocessor_config.json",
    "vocab.json",
    "vocab.txt",
];

fn select_tree_files(
    files: &[TreeFile],
    selection: ArtifactSelection,
) -> Result<(Vec<ExpectedFile>, ModelArtifactsDocument), ModelInputError> {
    let format = selection.format()?;
    let requested = std::iter::once(selection.primary.as_str())
        .chain(selection.auxiliary.iter().map(|file| file.path.as_str()))
        .collect::<BTreeSet<_>>();
    let mut selected = std::collections::BTreeMap::new();
    for file in files {
        let required_metadata = format == ModelArtifactFormat::Safetensors
            && SAFETENSORS_METADATA.contains(&file.path.as_str());
        if (requested.contains(file.path.as_str()) || required_metadata)
            && selected.insert(file.path.as_str(), file).is_some()
        {
            return Err(ModelInputError("Hub tree contains an ambiguous artifact"));
        }
    }
    if requested.iter().any(|path| !selected.contains_key(path)) {
        return Err(ModelInputError(
            "selected artifact is missing from the immutable Hub tree",
        ));
    }
    if format == ModelArtifactFormat::Safetensors && !selected.contains_key("config.json") {
        return Err(ModelInputError("safetensors metadata is incomplete"));
    }
    let artifacts = match selection.configured {
        Some(configured) => {
            verify_declared_file(
                &configured.primary.path,
                configured.primary.bytes,
                configured.primary.sha256.as_ref(),
                selected[configured.primary.path.as_str()],
            )?;
            for declared in &configured.auxiliary {
                verify_declared_file(
                    &declared.path,
                    declared.bytes,
                    declared.sha256.as_ref(),
                    selected[declared.path.as_str()],
                )?;
            }
            configured
        }
        None => ModelArtifactsDocument {
            schema: "sy.spark.model-artifacts/v2".into(),
            format,
            primary: artifact_file(selected[selection.primary.as_str()]),
            auxiliary: selection
                .auxiliary
                .iter()
                .map(|file| {
                    auxiliary_artifact_file(selected[file.path.as_str()], file.role.clone())
                })
                .collect(),
            quantization: (format == ModelArtifactFormat::Gguf)
                .then(|| {
                    ["Q4_K_XL", "Q4_K_M", "Q8_0"]
                        .into_iter()
                        .find(|value| selection.primary.contains(value))
                })
                .flatten()
                .map(str::to_owned),
            capabilities: vec!["text_generation".into()],
            configured_alias: None,
            engine_profile: None,
        },
    };
    let expected = selected
        .into_values()
        .map(TreeFile::expected)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((expected, artifacts))
}

fn artifact_file(file: &TreeFile) -> ModelArtifactFileDocument {
    ModelArtifactFileDocument {
        path: file.path.clone(),
        bytes: file.size,
        sha256: file.sha256.clone(),
    }
}

fn linked_sha256(tree_sha256: Option<String>, etag: &str) -> Option<String> {
    tree_sha256.or_else(|| {
        (etag.len() == 64 && etag.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| etag.to_ascii_lowercase())
    })
}

fn linked_headers(response: &reqwest::Response) -> Result<(u64, String), AcquisitionError> {
    let headers = response.headers();
    let size = headers
        .get("x-linked-size")
        .or_else(|| headers.get(reqwest::header::CONTENT_LENGTH))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| acquisition_error(TransferFailure::Other, "Hub file size is missing"))?;
    let etag = headers
        .get("x-linked-etag")
        .or_else(|| headers.get(reqwest::header::ETAG))
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .strip_prefix("W/")
                .unwrap_or(value)
                .trim_matches('"')
                .to_owned()
        })
        .ok_or_else(|| acquisition_error(TransferFailure::Other, "Hub file digest is missing"))?;
    Ok((size, etag))
}

fn auxiliary_artifact_file(
    file: &TreeFile,
    role: super::wire::ModelArtifactRole,
) -> ModelAuxiliaryArtifactDocument {
    ModelAuxiliaryArtifactDocument {
        role,
        path: file.path.clone(),
        bytes: file.size,
        sha256: file.sha256.clone(),
    }
}

fn artifact_identity_sha256(
    artifacts: &ModelArtifactsDocument,
) -> Result<String, serde_json::Error> {
    #[derive(serde::Serialize)]
    struct ArtifactIdentity<'a> {
        format: ModelArtifactFormat,
        primary: &'a ModelArtifactFileDocument,
        auxiliary: &'a [ModelAuxiliaryArtifactDocument],
    }
    let identity = ArtifactIdentity {
        format: artifacts.format,
        primary: &artifacts.primary,
        auxiliary: &artifacts.auxiliary,
    };
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&identity)?)
    ))
}

fn verify_declared_file(
    path: &str,
    bytes: u64,
    sha256: Option<&String>,
    resolved: &TreeFile,
) -> Result<(), ModelInputError> {
    if path != resolved.path
        || bytes != resolved.size
        || sha256.is_some_and(|hash| resolved.sha256.as_ref() != Some(hash))
    {
        return Err(ModelInputError(
            "catalog artifact differs from the immutable Hub tree",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct AcquisitionError {
    pub failure: TransferFailure,
    pub detail: &'static str,
}

impl fmt::Display for AcquisitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.detail)
    }
}

impl std::error::Error for AcquisitionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub current_bytes: u64,
    pub total_bytes: Option<u64>,
    pub file: Option<String>,
}

pub(super) fn progress_channel() -> (
    tokio::sync::mpsc::Sender<ProgressUpdate>,
    tokio::sync::mpsc::Receiver<ProgressUpdate>,
) {
    tokio::sync::mpsc::channel(MODEL_PROGRESS_QUEUE_CAPACITY)
}

#[derive(Debug, Clone)]
pub struct FallbackConfig {
    pub executable: PathBuf,
    pub credential: PathBuf,
}

pub struct HubAcquirer {
    cache: PathBuf,
    endpoint: String,
    token: Option<SecretString>,
    disk_reserve_bytes: u64,
    no_progress_timeout: std::time::Duration,
    fallback: Option<FallbackConfig>,
}

impl HubAcquirer {
    pub fn disk_reserve_bytes(&self) -> u64 {
        self.disk_reserve_bytes
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache
    }
    pub fn new(
        cache: PathBuf,
        endpoint: String,
        token: Option<SecretString>,
        disk_reserve_bytes: u64,
        no_progress_timeout: std::time::Duration,
        fallback: Option<FallbackConfig>,
    ) -> Result<Self, AcquisitionError> {
        let secure_endpoint = endpoint.starts_with("https://")
            || (cfg!(test)
                && (endpoint.starts_with("http://127.0.0.1:")
                    || endpoint.starts_with("http://[::1]:")));
        if !secure_endpoint || disk_reserve_bytes == 0 || no_progress_timeout.is_zero() {
            return Err(acquisition_error(
                TransferFailure::Policy,
                "model acquisition policy is invalid",
            ));
        }
        Ok(Self {
            cache,
            endpoint,
            token,
            disk_reserve_bytes,
            no_progress_timeout,
            fallback,
        })
    }

    async fn linked_file_metadata(
        &self,
        repository: &Repository,
        commit: &CommitSha,
        path: &str,
    ) -> Result<(u64, String), AcquisitionError> {
        let mut url = reqwest::Url::parse(&format!("{}/{}", self.endpoint, repository.as_str()))
            .map_err(|_| acquisition_error(TransferFailure::Policy, "Hub endpoint is invalid"))?;
        url.path_segments_mut()
            .map_err(|_| acquisition_error(TransferFailure::Policy, "Hub endpoint is invalid"))?
            .extend(["resolve", commit.as_str()])
            .extend(path.split('/'));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                acquisition_error(TransferFailure::Other, "Hugging Face request failed")
            })?;
        let mut request = client.head(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose_secret());
        }
        let response = request.send().await.map_err(|_| {
            acquisition_error(TransferFailure::Other, "Hugging Face request failed")
        })?;
        if !response.status().is_success() && !response.status().is_redirection() {
            return Err(acquisition_error(
                TransferFailure::Other,
                "Hugging Face request failed",
            ));
        }
        linked_headers(&response)
    }

    pub async fn resolve_selected(
        &self,
        repository: Repository,
        revision: Revision,
        selection: ArtifactSelection,
    ) -> Result<AcquisitionPlan, AcquisitionError> {
        fs::create_dir_all(&self.cache).map_err(|_| {
            acquisition_error(TransferFailure::DiskReserve, "model cache is unavailable")
        })?;
        let client = self.client()?;
        let (owner, name) = repository
            .as_str()
            .split_once('/')
            .expect("validated repository");
        let remote = client.model(owner, name);
        let info = remote
            .info()
            .revision(revision.as_str().to_owned())
            .send()
            .await
            .map_err(classify_hub_error)?;
        let commit = CommitSha::parse(info.sha.as_deref().ok_or_else(|| {
            acquisition_error(TransferFailure::Other, "Hub omitted the resolved commit")
        })?)
        .map_err(|_| acquisition_error(TransferFailure::Other, "Hub returned an invalid commit"))?;
        let automatic = selection.automatic;
        let selected_paths = std::iter::once(selection.primary.as_str())
            .chain(selection.auxiliary.iter().map(|file| file.path.as_str()))
            .collect::<BTreeSet<_>>();
        let stream = remote
            .list_tree()
            .revision(commit.as_str().to_owned())
            .recursive(true)
            .expand(true)
            .send()
            .map_err(classify_hub_error)?;
        futures_util::pin_mut!(stream);
        let mut tree = Vec::new();
        while let Some(entry) = stream.next().await {
            if let RepoTreeEntry::File {
                oid,
                size,
                path,
                lfs,
                ..
            } = entry.map_err(classify_hub_error)?
            {
                let tree_sha256 = lfs.and_then(|item| item.sha256);
                let sha256 = if tree_sha256.is_none() && selected_paths.contains(path.as_str()) {
                    let (linked_size, etag) = self
                        .linked_file_metadata(&repository, &commit, &path)
                        .await?;
                    (linked_size == size)
                        .then(|| linked_sha256(None, &etag))
                        .flatten()
                } else {
                    tree_sha256
                };
                let blob_key = sha256.as_deref().unwrap_or(&oid).to_owned();
                tree.push(TreeFile {
                    path,
                    size,
                    blob_key,
                    sha256,
                });
            }
        }
        let selection = if selection.automatic {
            infer_tree_selection(&tree)
        } else {
            Ok(selection)
        }
        .map_err(|_| {
            acquisition_error(TransferFailure::Policy, "model artifact inference failed")
        })?;
        let (expected, mut artifacts) = select_tree_files(&tree, selection).map_err(|_| {
            acquisition_error(
                TransferFailure::Policy,
                "artifact selection does not match the immutable Hub tree",
            )
        })?;
        if automatic {
            artifacts.capabilities = pipeline_capabilities(info.pipeline_tag.as_deref());
        }
        let logical_bytes = expected.iter().try_fold(0_u64, |sum, file| {
            sum.checked_add(file.size).ok_or_else(|| {
                acquisition_error(
                    TransferFailure::Policy,
                    "model size overflows byte accounting",
                )
            })
        })?;
        let blobs = self
            .cache
            .join(repository_cache_name(&repository))
            .join("blobs");
        let mut unique_bytes = 0_u64;
        for file in &expected {
            let path = file.path.to_string_lossy();
            let resolved = tree
                .iter()
                .find(|entry| entry.path == path)
                .expect("selected files originate in the resolved tree");
            if !snapshot_file_is_complete(
                &self.cache,
                &repository,
                &commit,
                &resolved.path,
                resolved.size,
            ) && !blobs.join(&resolved.blob_key).is_file()
            {
                unique_bytes = unique_bytes.checked_add(resolved.size).ok_or_else(|| {
                    acquisition_error(
                        TransferFailure::Policy,
                        "model size overflows byte accounting",
                    )
                })?;
            }
        }
        let temporary_bytes = unique_bytes.min(8 * 1024 * 1024 * 1024);
        self.enforce_disk_reserve(unique_bytes, temporary_bytes)?;
        let license = info
            .tags
            .unwrap_or_default()
            .into_iter()
            .find_map(|tag| tag.strip_prefix("license:").map(str::to_owned));
        Ok(AcquisitionPlan {
            repository,
            commit,
            expected,
            logical_bytes,
            unique_bytes,
            temporary_bytes,
            gated: info
                .gated
                .is_some_and(|value| value != serde_json::Value::Bool(false)),
            license,
            artifacts,
        })
    }

    pub async fn acquire(
        &self,
        plan: &AcquisitionPlan,
        alias: Option<Alias>,
        progress: tokio::sync::mpsc::Sender<ProgressUpdate>,
    ) -> Result<ModelDocument, AcquisitionError> {
        let primary = self
            .download_rust(plan, progress.clone())
            .await
            .and_then(|()| {
                verify_snapshot(&self.cache, &plan.repository, &plan.commit, &plan.expected)
                    .map(|_| ())
                    .map_err(|_| {
                        acquisition_error(
                            TransferFailure::XetIntegrity,
                            "downloaded snapshot failed independent verification",
                        )
                    })
            });
        let mut transport = "rust-xet";
        if let Err(error) = primary {
            if !should_run_fallback(error.failure, 0) {
                return Err(error);
            }
            self.download_fallback(plan, progress).await?;
            transport = "python-http-fallback";
        }
        let verified = verify_snapshot(&self.cache, &plan.repository, &plan.commit, &plan.expected)
            .map_err(|_| {
                acquisition_error(
                    TransferFailure::XetIntegrity,
                    "downloaded snapshot failed independent verification",
                )
            })?;
        let mut canonical = format!(
            "huggingface:{}@{}",
            plan.repository.as_str(),
            plan.commit.as_str()
        );
        canonical.push_str("#sha256:");
        canonical.push_str(&artifact_identity_sha256(&plan.artifacts).map_err(|_| {
            acquisition_error(
                TransferFailure::Policy,
                "artifact identity cannot be canonicalized",
            )
        })?);
        let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        Ok(ModelDocument {
            schema: MODEL_SCHEMA.into(),
            id: format!("m_{}", &digest[..32]),
            canonical,
            repository: plan.repository.as_str().into(),
            commit: plan.commit.as_str().into(),
            snapshot: verified
                .path
                .strip_prefix(&self.cache)
                .map_err(|_| {
                    acquisition_error(
                        TransferFailure::Policy,
                        "verified snapshot escaped its cache",
                    )
                })?
                .to_string_lossy()
                .into_owned(),
            artifacts: Some(plan.artifacts.clone()),
            logical_bytes: verified.logical_bytes,
            unique_bytes: plan.unique_bytes,
            aliases: alias
                .map(|value| vec![value.as_str().into()])
                .unwrap_or_default(),
            active_instances: Vec::new(),
            transport: transport.into(),
            verified_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            gated: plan.gated,
            license: plan.license.clone(),
        })
    }

    fn client(&self) -> Result<hf_hub::HFClient, AcquisitionError> {
        let builder = hf_hub::HFClient::builder()
            .endpoint(&self.endpoint)
            .cache_dir(&self.cache)
            .cache_enabled(true)
            .retry_max_attempts(3);
        let builder = match &self.token {
            Some(token) => builder.token(token.expose_secret()),
            None => builder,
        };
        builder.build().map_err(classify_hub_error)
    }

    async fn download_rust(
        &self,
        plan: &AcquisitionPlan,
        progress: tokio::sync::mpsc::Sender<ProgressUpdate>,
    ) -> Result<(), AcquisitionError> {
        let client = self.client()?;
        let (owner, name) = plan
            .repository
            .as_str()
            .split_once('/')
            .expect("validated repository");
        let heartbeat = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let handler = DownloadProgress {
            heartbeat: std::sync::Arc::clone(&heartbeat),
            sender: progress,
        };
        let remote = client.model(owner, name);
        let allow_patterns = plan
            .expected
            .iter()
            .map(|file| file.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let download = remote
            .snapshot_download()
            .revision(plan.commit.as_str().to_owned())
            .allow_patterns(allow_patterns)
            .max_workers(4)
            .progress(handler)
            .send();
        tokio::pin!(download);
        loop {
            tokio::select! {
                result = &mut download => return result.map(|_| ()).map_err(classify_hub_error),
                () = tokio::time::sleep(self.no_progress_timeout) => {
                    let stale = heartbeat.lock().map(|seen| seen.elapsed() >= self.no_progress_timeout).unwrap_or(true);
                    if stale {
                        return Err(acquisition_error(TransferFailure::NoProgress, "Rust Hub transfer made no bounded progress"));
                    }
                }
            }
        }
    }

    async fn download_fallback(
        &self,
        plan: &AcquisitionPlan,
        progress: tokio::sync::mpsc::Sender<ProgressUpdate>,
    ) -> Result<(), AcquisitionError> {
        let fallback = self.fallback.as_ref().ok_or_else(|| {
            acquisition_error(
                TransferFailure::XetTransport,
                "classified Xet failure has no installed HTTP fallback",
            )
        })?;
        let mut command = tokio::process::Command::new(&fallback.executable);
        command
            .env_clear()
            .env("HF_HUB_DISABLE_XET", "1")
            .env("HF_HUB_CACHE", &self.cache)
            .env("HF_TOKEN_PATH", &fallback.credential)
            .env("TMPDIR", "/tmp")
            .arg("download")
            .arg(plan.repository.as_str());
        for file in &plan.expected {
            command.arg(&file.path);
        }
        command
            .args(["--revision", plan.commit.as_str(), "--cache-dir"])
            .arg(&self.cache)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| {
            acquisition_error(
                TransferFailure::XetTransport,
                "contained HTTP fallback failed",
            )
        })?;
        let mut seen = repository_blob_bytes(&self.cache, &plan.repository).unwrap_or_default();
        let mut last_progress = std::time::Instant::now();
        let _ = progress.try_send(ProgressUpdate {
            current_bytes: seen.min(plan.logical_bytes),
            total_bytes: Some(plan.logical_bytes),
            file: None,
        });
        loop {
            tokio::select! {
                status = child.wait() => return match status {
                    Ok(status) if status.success() => Ok(()),
                    _ => Err(acquisition_error(TransferFailure::XetTransport, "contained HTTP fallback failed")),
                },
                () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    let current = repository_blob_bytes(&self.cache, &plan.repository)
                        .map_err(|_| acquisition_error(TransferFailure::XetIntegrity, "fallback cache progress cannot be verified"))?;
                    if current > seen {
                        seen = current;
                        last_progress = std::time::Instant::now();
                        let _ = progress.try_send(ProgressUpdate {
                            current_bytes: current.min(plan.logical_bytes),
                            total_bytes: Some(plan.logical_bytes),
                            file: None,
                        });
                    } else if last_progress.elapsed() >= self.no_progress_timeout {
                        return Err(acquisition_error(TransferFailure::NoProgress, "contained HTTP fallback made no bounded progress"));
                    }
                }
            }
        }
    }

    fn enforce_disk_reserve(&self, unique: u64, temporary: u64) -> Result<(), AcquisitionError> {
        let stats = rustix::fs::statvfs(&self.cache).map_err(|_| {
            acquisition_error(
                TransferFailure::DiskReserve,
                "available model-cache space is unknown",
            )
        })?;
        let available = stats.f_bavail.checked_mul(stats.f_frsize).ok_or_else(|| {
            acquisition_error(
                TransferFailure::DiskReserve,
                "available model-cache space overflowed",
            )
        })?;
        let required = unique
            .checked_add(temporary)
            .and_then(|bytes| bytes.checked_add(self.disk_reserve_bytes))
            .ok_or_else(|| {
                acquisition_error(TransferFailure::DiskReserve, "model disk plan overflowed")
            })?;
        if available < required {
            return Err(acquisition_error(
                TransferFailure::DiskReserve,
                "model download would violate the disk reserve",
            ));
        }
        Ok(())
    }
}

pub fn execute_removal(plan: &RemovalPlan) -> Result<(), ModelInputError> {
    for blob in &plan.removable_blobs {
        fs::remove_file(blob)
            .map_err(|_| ModelInputError("unreferenced blob could not be removed"))?;
    }
    fs::remove_dir_all(&plan.snapshot).map_err(|_| ModelInputError("snapshot could not be removed"))
}

#[derive(Clone)]
struct DownloadProgress {
    heartbeat: std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
    sender: tokio::sync::mpsc::Sender<ProgressUpdate>,
}

impl hf_hub::progress::ProgressHandler for DownloadProgress {
    fn on_progress(&self, event: &hf_hub::progress::ProgressEvent) {
        use hf_hub::progress::{DownloadEvent, ProgressEvent};
        let update = match event {
            ProgressEvent::Download(DownloadEvent::Start { total_bytes, .. }) => {
                Some(ProgressUpdate {
                    current_bytes: 0,
                    total_bytes: Some(*total_bytes),
                    file: None,
                })
            }
            ProgressEvent::Download(DownloadEvent::AggregateProgress {
                bytes_completed,
                total_bytes,
                ..
            }) => Some(ProgressUpdate {
                current_bytes: *bytes_completed,
                total_bytes: Some(*total_bytes),
                file: None,
            }),
            ProgressEvent::Download(DownloadEvent::Progress { files }) => {
                files.last().map(|file| ProgressUpdate {
                    current_bytes: file.bytes_completed,
                    total_bytes: Some(file.total_bytes),
                    file: Some(file.filename.clone()),
                })
            }
            ProgressEvent::Download(DownloadEvent::Complete) => None,
            ProgressEvent::Upload(_) => None,
        };
        if let Ok(mut heartbeat) = self.heartbeat.lock() {
            *heartbeat = std::time::Instant::now();
        }
        if let Some(update) = update {
            let _ = self.sender.try_send(update);
        }
    }
}

fn classify_hub_error(error: hf_hub::HFError) -> AcquisitionError {
    let failure = match error {
        hf_hub::HFError::AuthRequired { .. } | hf_hub::HFError::Forbidden { .. } => {
            TransferFailure::Authentication
        }
        hf_hub::HFError::RepoNotFound { .. }
        | hf_hub::HFError::RevisionNotFound { .. }
        | hf_hub::HFError::EntryNotFound { .. } => TransferFailure::NotFound,
        hf_hub::HFError::Xet { .. } => TransferFailure::XetTransport,
        _ => TransferFailure::Other,
    };
    acquisition_error(failure, "Hugging Face request failed")
}

fn acquisition_error(failure: TransferFailure, detail: &'static str) -> AcquisitionError {
    AcquisitionError { failure, detail }
}

#[cfg(test)]
mod tests {
    #[test]
    fn linked_etag_recovers_missing_tree_lfs_sha256() {
        const SHA256: &str = "3f227079003add2511437e5b1e94812e363385225bf6a9b47b0054a72bc8b01e";
        assert_eq!(super::linked_sha256(None, SHA256), Some(SHA256.into()));
        assert_eq!(super::linked_sha256(None, "git-oid"), None);
    }

    use super::{
        plan_removal, should_run_fallback, verify_snapshot, Alias, CommitSha, ExpectedFile,
        Repository, Revision, TransferFailure,
    };
    use sha2::Digest;
    use std::{
        fs,
        os::unix::fs::{symlink, MetadataExt},
    };

    fn auxiliary(
        role: crate::spark::wire::ModelArtifactRole,
        path: &str,
    ) -> crate::spark::wire::ModelArtifactSelectorDocument {
        crate::spark::wire::ModelArtifactSelectorDocument {
            role,
            path: path.into(),
        }
    }

    #[test]
    fn repository_revision_and_alias_validation_resists_traversal() {
        assert!(Repository::parse("ornith-ai/Ornith-1.5-9B").is_ok());
        assert!(Revision::parse("refs/pr/12").is_ok());
        assert!(CommitSha::parse("489cb97981b8654bcfcf30ce1f94ed1b62e07b53").is_ok());
        assert!(Alias::parse("ornith-1.5:9b").is_ok());

        for invalid in [
            "",
            "owner",
            "/model",
            "owner/../model",
            "owner/model/extra",
            "owner\\model",
        ] {
            assert!(
                Repository::parse(invalid).is_err(),
                "accepted repository {invalid:?}"
            );
        }
        for invalid in [
            "",
            ".",
            "..",
            "../main",
            "refs//main",
            "main\\other",
            "main\nnext",
        ] {
            assert!(
                Revision::parse(invalid).is_err(),
                "accepted revision {invalid:?}"
            );
        }
        for invalid in ["main", "489cb97981b8654bcfcf30ce1f94ed1b62e07b53x"] {
            assert!(
                CommitSha::parse(invalid).is_err(),
                "accepted commit {invalid:?}"
            );
        }
        for invalid in [
            "ornith",
            ":9b",
            "ornith:",
            "Ornith:9b",
            "ornith:../9b",
            "ornith:9b:latest",
        ] {
            assert!(Alias::parse(invalid).is_err(), "accepted alias {invalid:?}");
        }
    }

    #[test]
    fn verification_descriptor_resolves_every_symlink_inside_repo_cache() {
        const CONTENT: &[u8] = b"verified model bytes";
        let root = tempfile::tempdir().unwrap();
        let repository = Repository::parse("owner/model").unwrap();
        let commit = CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap();
        let repo = root.path().join("models--owner--model");
        let snapshot = repo.join("snapshots").join(commit.as_str());
        fs::create_dir_all(&snapshot).unwrap();
        fs::create_dir_all(repo.join("blobs")).unwrap();
        fs::write(repo.join("blobs/blob"), CONTENT).unwrap();
        symlink("../../blobs/blob", snapshot.join("config.json")).unwrap();
        let expected = vec![ExpectedFile::new(
            "config.json",
            CONTENT.len() as u64,
            Some(format!("{:x}", sha2::Sha256::digest(CONTENT))),
        )
        .unwrap()];

        assert!(verify_snapshot(root.path(), &repository, &commit, &expected).is_ok());
        fs::write(repo.join("blobs/blob.incomplete"), b"partial").unwrap();
        assert!(verify_snapshot(root.path(), &repository, &commit, &expected).is_err());
        fs::remove_file(repo.join("blobs/blob.incomplete")).unwrap();
        fs::remove_file(snapshot.join("config.json")).unwrap();
        symlink("/etc/passwd", snapshot.join("config.json")).unwrap();
        assert!(verify_snapshot(root.path(), &repository, &commit, &expected).is_err());
    }

    #[test]
    fn fallback_runs_once_only_for_classified_xet_failures() {
        for failure in [
            TransferFailure::XetTransport,
            TransferFailure::XetIntegrity,
            TransferFailure::NoProgress,
        ] {
            assert!(should_run_fallback(failure, 0));
            assert!(!should_run_fallback(failure, 1));
        }
        for failure in [
            TransferFailure::Authentication,
            TransferFailure::NotFound,
            TransferFailure::Policy,
            TransferFailure::DiskReserve,
            TransferFailure::Cancelled,
        ] {
            assert!(!should_run_fallback(failure, 0));
        }
    }

    #[test]
    fn remove_plan_counts_unique_bytes_and_refuses_active_snapshot() {
        const SHARED_BYTES: &[u8] = b"shared";
        const UNIQUE_BYTES: &[u8] = b"unique target";
        let root = tempfile::tempdir().unwrap();
        let repository = Repository::parse("owner/model").unwrap();
        let target = CommitSha::parse("1111111111111111111111111111111111111111").unwrap();
        let retained = CommitSha::parse("2222222222222222222222222222222222222222").unwrap();
        let repo = root.path().join("models--owner--model");
        fs::create_dir_all(repo.join("blobs")).unwrap();
        fs::write(repo.join("blobs/shared"), SHARED_BYTES).unwrap();
        fs::write(repo.join("blobs/unique"), UNIQUE_BYTES).unwrap();
        for commit in [&target, &retained] {
            fs::create_dir_all(repo.join("snapshots").join(commit.as_str())).unwrap();
            symlink(
                "../../blobs/shared",
                repo.join("snapshots")
                    .join(commit.as_str())
                    .join("shared.bin"),
            )
            .unwrap();
        }
        symlink(
            "../../blobs/unique",
            repo.join("snapshots")
                .join(target.as_str())
                .join("unique.bin"),
        )
        .unwrap();

        assert!(plan_removal(root.path(), &repository, &target, true).is_err());
        let plan = plan_removal(root.path(), &repository, &target, false).unwrap();
        assert_eq!(plan.reclaimable_bytes, UNIQUE_BYTES.len() as u64);
        assert_eq!(plan.removable_blobs.len(), 1);
    }

    #[test]
    fn partial_snapshot_never_promotes_and_resume_reuses_blobs() {
        const CONTENT: &[u8] = b"resumable bytes";
        let root = tempfile::tempdir().unwrap();
        let repository = Repository::parse("owner/resume-model").unwrap();
        let commit = CommitSha::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let repo = root.path().join("models--owner--resume-model");
        let snapshot = repo.join("snapshots").join(commit.as_str());
        fs::create_dir_all(repo.join("blobs")).unwrap();
        fs::create_dir_all(&snapshot).unwrap();
        let partial = repo.join("blobs/model.incomplete");
        fs::write(&partial, CONTENT).unwrap();
        assert_eq!(
            super::repository_blob_bytes(root.path(), &repository).unwrap(),
            CONTENT.len() as u64
        );
        let expected = [ExpectedFile::new("model.bin", CONTENT.len() as u64, None).unwrap()];
        assert!(verify_snapshot(root.path(), &repository, &commit, &expected).is_err());
        let blob = repo.join("blobs/model");
        fs::rename(&partial, &blob).unwrap();
        symlink("../../blobs/model", snapshot.join("model.bin")).unwrap();
        let before = fs::metadata(&blob).unwrap().ino();
        assert!(verify_snapshot(root.path(), &repository, &commit, &expected).is_ok());
        assert_eq!(fs::metadata(&blob).unwrap().ino(), before);
    }

    #[test]
    fn arbitrary_control_and_separator_bytes_never_form_model_names() {
        for byte in 0_u8..=u8::MAX {
            if byte.is_ascii_control() || matches!(byte, b'/' | b'\\' | b':') {
                let value = format!("safe{}name", char::from(byte));
                assert!(Alias::parse(&format!("{value}:tag")).is_err());
            }
        }
    }

    #[test]
    fn progress_queue_is_bounded() {
        let (sender, _receiver) = super::progress_channel();
        assert_eq!(sender.max_capacity(), super::MODEL_PROGRESS_QUEUE_CAPACITY);
    }

    #[test]
    fn plan_selects_primary_auxiliary_and_required_metadata_only() {
        let selection = super::ArtifactSelection::generic(
            "model.safetensors.index.json",
            vec![
                auxiliary(
                    crate::spark::wire::ModelArtifactRole::WeightShard,
                    "model-00001-of-00002.safetensors",
                ),
                auxiliary(
                    crate::spark::wire::ModelArtifactRole::WeightShard,
                    "model-00002-of-00002.safetensors",
                ),
            ],
        )
        .unwrap();
        let files = [
            ("config.json", 11),
            ("tokenizer.json", 12),
            ("generation_config.json", 13),
            ("model.safetensors.index.json", 14),
            ("model-00001-of-00002.safetensors", 15),
            ("model-00002-of-00002.safetensors", 16),
            ("other-Q4.gguf", 17),
            ("README.md", 18),
        ]
        .into_iter()
        .map(super::TreeFile::fixture)
        .collect::<Vec<_>>();

        let (expected, artifacts) = super::select_tree_files(&files, selection).unwrap();

        assert_eq!(
            expected
                .iter()
                .map(|file| file.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "config.json",
                "generation_config.json",
                "model-00001-of-00002.safetensors",
                "model-00002-of-00002.safetensors",
                "model.safetensors.index.json",
                "tokenizer.json",
            ]
        );
        assert_eq!(artifacts.primary.path, "model.safetensors.index.json");
    }

    #[test]
    fn automatic_selection_prefers_a_complete_safetensors_snapshot() {
        let files = [
            ("config.json", 11),
            ("model.safetensors.index.json", 12),
            ("model-00001-of-00002.safetensors", 13),
            ("model-00002-of-00002.safetensors", 14),
            ("model-Q4_K_XL.gguf", 15),
        ]
        .into_iter()
        .map(super::TreeFile::fixture)
        .collect::<Vec<_>>();
        let selection = super::infer_tree_selection(&files).unwrap();
        let (_, artifacts) = super::select_tree_files(&files, selection).unwrap();
        assert_eq!(artifacts.primary.path, "model.safetensors.index.json");
        assert_eq!(artifacts.auxiliary.len(), 2);
    }

    #[test]
    fn automatic_gguf_selection_uses_the_spark_quantization_default() {
        let files = [("model-Q8_0.gguf", 11), ("model-Q4_K_XL.gguf", 12)]
            .into_iter()
            .map(super::TreeFile::fixture)
            .collect::<Vec<_>>();
        let selection = super::infer_tree_selection(&files).unwrap();
        let (_, artifacts) = super::select_tree_files(&files, selection).unwrap();
        assert_eq!(artifacts.primary.path, "model-Q4_K_XL.gguf");
    }

    #[test]
    fn hub_pipeline_metadata_distinguishes_embeddings_from_generation() {
        assert_eq!(
            super::pipeline_capabilities(Some("feature-extraction")),
            ["text_embeddings"]
        );
        assert_eq!(
            super::pipeline_capabilities(Some("text-generation")),
            ["text_generation"]
        );
    }

    #[test]
    fn missing_or_ambiguous_artifact_fails_before_download() {
        let files = vec![super::TreeFile::fixture(("model.gguf", 7))];
        let missing = super::ArtifactSelection::generic("absent.gguf", Vec::new()).unwrap();
        assert!(super::select_tree_files(&files, missing).is_err());

        let ambiguous = super::ArtifactSelection::generic(
            "model.gguf",
            [auxiliary(
                crate::spark::wire::ModelArtifactRole::WeightShard,
                "model.gguf",
            )],
        );
        assert!(ambiguous.is_err());
        assert!(super::ArtifactSelection::generic("../model.gguf", Vec::new()).is_err());

        let declared: crate::spark::wire::ModelArtifactsDocument = serde_json::from_str(
            r#"{"schema":"sy.spark.model-artifacts/v2","format":"gguf","primary":{"path":"model.gguf","bytes":8,"sha256":null},"auxiliary":[],"quantization":"Q4","capabilities":["text_generation"],"configured_alias":"model:q4"}"#,
        )
        .unwrap();
        let mismatched = super::ArtifactSelection::configured(declared).unwrap();
        assert!(super::select_tree_files(&files, mismatched).is_err());
    }

    #[test]
    fn model_identity_changes_with_the_auxiliary_artifact_set() {
        let first: crate::spark::wire::ModelArtifactsDocument = serde_json::from_str(
            r#"{"schema":"sy.spark.model-artifacts/v2","format":"gguf","primary":{"path":"model.gguf","bytes":8,"sha256":null},"auxiliary":[{"role":"projector","path":"vision-a.gguf","bytes":4,"sha256":null}],"quantization":"Q4","capabilities":["text_generation","vision"],"configured_alias":"model:q4"}"#,
        )
        .unwrap();
        let mut second = first.clone();
        second.auxiliary[0].path = "vision-b.gguf".into();

        assert_ne!(
            super::artifact_identity_sha256(&first).unwrap(),
            super::artifact_identity_sha256(&second).unwrap()
        );
        second.auxiliary[0].path = first.auxiliary[0].path.clone();
        second.auxiliary[0].role = crate::spark::wire::ModelArtifactRole::WeightShard;
        assert_ne!(
            super::artifact_identity_sha256(&first).unwrap(),
            super::artifact_identity_sha256(&second).unwrap()
        );
    }

    #[test]
    fn split_gguf_selection_keeps_weight_shards_as_verified_inputs() {
        let selection = super::ArtifactSelection::generic(
            "model-00001-of-00002.gguf",
            [auxiliary(
                crate::spark::wire::ModelArtifactRole::WeightShard,
                "model-00002-of-00002.gguf",
            )],
        )
        .unwrap();
        let files = [
            ("model-00001-of-00002.gguf", 7),
            ("model-00002-of-00002.gguf", 11),
            ("other.gguf", 13),
        ]
        .into_iter()
        .map(super::TreeFile::fixture)
        .collect::<Vec<_>>();

        let (expected, artifacts) = super::select_tree_files(&files, selection).unwrap();

        assert_eq!(expected.len(), 2);
        assert_eq!(
            artifacts.auxiliary[0].role,
            crate::spark::wire::ModelArtifactRole::WeightShard
        );
    }

    #[tokio::test]
    async fn rust_hub_selective_download_verifies_the_native_cache_end_to_end() {
        use axum::{
            body::Body,
            http::{header, HeaderValue, Response},
            routing::get,
            Router,
        };
        const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
        const CONTENT: &[u8] = b"hermetic model";
        async fn info() -> axum::Json<serde_json::Value> {
            axum::Json(
                serde_json::json!({"id":"owner/model","sha":COMMIT,"gated":false,"tags":["license:mit"]}),
            )
        }
        async fn tree() -> axum::Json<serde_json::Value> {
            axum::Json(serde_json::json!([
                {"type":"file","oid":"git-tree-oid","size":CONTENT.len(),"path":"model.gguf","lfs":null},
                {"type":"file","oid":"unused-tree-oid","size":CONTENT.len(),"path":"other-Q2.gguf","lfs":null}
            ]))
        }
        async fn file() -> Response<Body> {
            let mut response = Response::new(Body::from(CONTENT));
            response
                .headers_mut()
                .insert(header::ETAG, HeaderValue::from_static("\"hermetic-etag\""));
            response
                .headers_mut()
                .insert("x-repo-commit", HeaderValue::from_static(COMMIT));
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from_static("14"));
            response
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/api/models/owner/model/revision/main", get(info))
                    .route(&format!("/api/models/owner/model/tree/{COMMIT}"), get(tree))
                    .route(
                        &format!("/owner/model/resolve/{COMMIT}/model.gguf"),
                        get(file),
                    ),
            )
            .await
            .unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let acquirer = super::HubAcquirer::new(
            root.path().to_owned(),
            format!("http://{address}"),
            None,
            1,
            std::time::Duration::from_secs(5),
            None,
        )
        .unwrap();
        let plan = acquirer
            .resolve_selected(
                Repository::parse("owner/model").unwrap(),
                Revision::parse("main").unwrap(),
                super::ArtifactSelection::generic("model.gguf", Vec::new()).unwrap(),
            )
            .await
            .unwrap();
        let (send, _receive) = super::progress_channel();
        let model = acquirer.acquire(&plan, None, send).await.unwrap();
        assert_eq!(
            (model.commit.as_str(), model.logical_bytes),
            (COMMIT, CONTENT.len() as u64)
        );
        assert_eq!(model.artifacts.unwrap().primary.path, "model.gguf");
        assert!(!root
            .path()
            .join("models--owner--model/snapshots")
            .join(COMMIT)
            .join("other-Q2.gguf")
            .exists());
        let resumed = acquirer
            .resolve_selected(
                Repository::parse("owner/model").unwrap(),
                Revision::parse("main").unwrap(),
                super::ArtifactSelection::generic("model.gguf", Vec::new()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resumed.unique_bytes, 0);
        server.abort();
    }
}
