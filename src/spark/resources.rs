//! Unified-memory admission and independent emergency-guard policy.

#[cfg(feature = "spark-agent")]
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

#[cfg(feature = "spark-agent")]
pub const GIB_BYTES: u64 = 1024 * 1024 * 1024;

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcePolicyConfig {
    pub system_reserve_gib: u64,
    pub emergency_available_floor_gib: u64,
    pub disk_reserve_gib: u64,
    pub startup_guard_interval_ms: u64,
    pub steady_guard_interval_ms: u64,
    pub emergency_consecutive_samples: u32,
    pub memory_full_psi_avg10_percent: f64,
}

#[cfg(feature = "spark-agent")]
impl ResourcePolicyConfig {
    pub fn policy(&self) -> Result<ResourcePolicy, &'static str> {
        let policy = ResourcePolicy {
            system_reserve_bytes: self
                .system_reserve_gib
                .checked_mul(GIB_BYTES)
                .ok_or("system reserve overflows bytes")?,
            emergency_available_floor_bytes: self
                .emergency_available_floor_gib
                .checked_mul(GIB_BYTES)
                .ok_or("emergency floor overflows bytes")?,
            disk_reserve_bytes: self
                .disk_reserve_gib
                .checked_mul(GIB_BYTES)
                .ok_or("disk reserve overflows bytes")?,
            startup_guard_interval_ms: self.startup_guard_interval_ms,
            steady_guard_interval_ms: self.steady_guard_interval_ms,
            emergency_consecutive_samples: self.emergency_consecutive_samples,
            memory_full_psi_avg10_percent: self.memory_full_psi_avg10_percent,
        };
        policy.validate()?;
        Ok(policy)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourcePolicy {
    pub system_reserve_bytes: u64,
    pub emergency_available_floor_bytes: u64,
    pub disk_reserve_bytes: u64,
    pub startup_guard_interval_ms: u64,
    pub steady_guard_interval_ms: u64,
    pub emergency_consecutive_samples: u32,
    pub memory_full_psi_avg10_percent: f64,
}

impl ResourcePolicy {
    #[cfg(all(test, feature = "spark-agent"))]
    pub const fn capacity_first() -> Self {
        Self {
            system_reserve_bytes: 8 * GIB_BYTES,
            emergency_available_floor_bytes: 8 * GIB_BYTES,
            disk_reserve_bytes: 100 * GIB_BYTES,
            startup_guard_interval_ms: 500,
            steady_guard_interval_ms: 2_000,
            emergency_consecutive_samples: 3,
            memory_full_psi_avg10_percent: 2.0,
        }
    }

    #[cfg(feature = "spark-agent")]
    pub fn max_snapshot_age_ms(&self) -> u64 {
        self.steady_guard_interval_ms.saturating_mul(2)
    }

    #[cfg(feature = "spark-agent")]
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.system_reserve_bytes < 8 * GIB_BYTES
            || self.emergency_available_floor_bytes < 8 * GIB_BYTES
            || self.disk_reserve_bytes < 100 * GIB_BYTES
            || self.startup_guard_interval_ms == 0
            || self.steady_guard_interval_ms == 0
            || self.emergency_consecutive_samples == 0
            || !self.memory_full_psi_avg10_percent.is_finite()
            || self.memory_full_psi_avg10_percent <= 0.0
        {
            Err("Spark resource policy is outside the accepted safety floor")
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct HostResourceSnapshot {
    pub schema: String,
    pub observed_at_unix_ms: u64,
    pub mem_total_bytes: Option<u64>,
    pub mem_available_bytes: Option<u64>,
    pub memory_full_psi_avg10_percent: Option<f64>,
    pub swap_in_pages_delta: Option<u64>,
    pub disk_available_bytes: Option<u64>,
}

impl HostResourceSnapshot {
    #[cfg(feature = "spark-agent")]
    pub fn is_complete(&self) -> bool {
        self.mem_total_bytes.is_some()
            && self.mem_available_bytes.is_some()
            && self.memory_full_psi_avg10_percent.is_some()
            && self.swap_in_pages_delta.is_some()
            && self.disk_available_bytes.is_some()
    }

    #[cfg(all(test, feature = "spark-agent"))]
    fn complete(observed_at_unix_ms: u64, total: u64, available: u64, disk: u64) -> Self {
        Self {
            schema: "sy.spark.resources.snapshot/v1".into(),
            observed_at_unix_ms,
            mem_total_bytes: Some(total),
            mem_available_bytes: Some(available),
            memory_full_psi_avg10_percent: Some(0.0),
            swap_in_pages_delta: Some(0),
            disk_available_bytes: Some(disk),
        }
    }
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleError {
    Unavailable,
    Invalid,
    Overflow,
}

#[cfg(feature = "spark-agent")]
pub trait HostSampler: Send {
    fn sample(&mut self) -> Result<HostResourceSnapshot, SampleError>;
}

#[cfg(feature = "spark-agent")]
pub struct ProcfsHostSampler {
    meminfo_path: std::path::PathBuf,
    pressure_path: std::path::PathBuf,
    vmstat_path: std::path::PathBuf,
    disk_path: std::path::PathBuf,
    previous_swap_in_pages: Option<u64>,
}

#[cfg(feature = "spark-agent")]
impl ProcfsHostSampler {
    pub fn production() -> Self {
        Self {
            meminfo_path: "/proc/meminfo".into(),
            pressure_path: "/proc/pressure/memory".into(),
            vmstat_path: "/proc/vmstat".into(),
            disk_path: "/var/lib/sy-spark".into(),
            previous_swap_in_pages: None,
        }
    }

    #[cfg(test)]
    fn with_paths(root: &Path) -> Self {
        Self {
            meminfo_path: root.join("meminfo"),
            pressure_path: root.join("pressure"),
            vmstat_path: root.join("vmstat"),
            disk_path: root.into(),
            previous_swap_in_pages: None,
        }
    }
}

#[cfg(feature = "spark-agent")]
impl HostSampler for ProcfsHostSampler {
    fn sample(&mut self) -> Result<HostResourceSnapshot, SampleError> {
        let meminfo =
            std::fs::read_to_string(&self.meminfo_path).map_err(|_| SampleError::Unavailable)?;
        let pressure =
            std::fs::read_to_string(&self.pressure_path).map_err(|_| SampleError::Unavailable)?;
        let vmstat =
            std::fs::read_to_string(&self.vmstat_path).map_err(|_| SampleError::Unavailable)?;
        let total = kib_value(&meminfo, "MemTotal:")?;
        let available = kib_value(&meminfo, "MemAvailable:")?;
        let psi = psi_full_avg10(&pressure)?;
        let swap_in = scalar_value(&vmstat, "pswpin")?;
        let swap_delta = self
            .previous_swap_in_pages
            .map(|previous| swap_in.saturating_sub(previous));
        self.previous_swap_in_pages = Some(swap_in);
        let stat = rustix::fs::statvfs(&self.disk_path).map_err(|_| SampleError::Unavailable)?;
        let disk_available = stat
            .f_bavail
            .checked_mul(stat.f_frsize)
            .ok_or(SampleError::Overflow)?;
        Ok(HostResourceSnapshot {
            schema: "sy.spark.resources.snapshot/v1".into(),
            observed_at_unix_ms: unix_millis(),
            mem_total_bytes: Some(total),
            mem_available_bytes: Some(available),
            memory_full_psi_avg10_percent: Some(psi),
            swap_in_pages_delta: swap_delta,
            disk_available_bytes: Some(disk_available),
        })
    }
}

#[cfg(feature = "spark-agent")]
fn kib_value(text: &str, key: &str) -> Result<u64, SampleError> {
    scalar_value(text, key)?
        .checked_mul(1024)
        .ok_or(SampleError::Overflow)
}

#[cfg(feature = "spark-agent")]
fn scalar_value(text: &str, key: &str) -> Result<u64, SampleError> {
    text.lines()
        .find_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            (fields.next()? == key).then(|| fields.next()?.parse::<u64>().ok())?
        })
        .ok_or(SampleError::Invalid)
}

#[cfg(feature = "spark-agent")]
fn psi_full_avg10(text: &str) -> Result<f64, SampleError> {
    text.lines()
        .find(|line| line.starts_with("full "))
        .and_then(|line| {
            line.split_ascii_whitespace()
                .find_map(|field| field.strip_prefix("avg10="))
        })
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or(SampleError::Invalid)
}

#[cfg(feature = "spark-agent")]
pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredEnvelope {
    pub instance_id: String,
    pub instance_name: String,
    pub cold_start_peak_bytes: u64,
}

#[cfg(feature = "spark-agent")]
impl DeclaredEnvelope {
    #[cfg(test)]
    pub fn new(instance_name: impl Into<String>, cold_start_peak_bytes: u64) -> Self {
        let instance_name = instance_name.into();
        Self {
            instance_id: instance_name.clone(),
            instance_name,
            cold_start_peak_bytes,
        }
    }
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateEnvelope {
    pub instance_name: String,
    pub cold_start_peak_bytes: u64,
    pub incremental_start_peak_bytes: u64,
    pub required_disk_bytes: u64,
}

#[cfg(feature = "spark-agent")]
impl CandidateEnvelope {
    pub fn new(
        instance_name: impl Into<String>,
        cold_start_peak_bytes: u64,
        incremental_start_peak_bytes: u64,
        required_disk_bytes: u64,
    ) -> Self {
        Self {
            instance_name: instance_name.into(),
            cold_start_peak_bytes,
            incremental_start_peak_bytes,
            required_disk_bytes,
        }
    }
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionRequest {
    pub desired: Vec<DeclaredEnvelope>,
    pub candidate: CandidateEnvelope,
    pub compatibility_verified: bool,
    pub guard_healthy: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmissionReport {
    pub schema: String,
    pub admitted: bool,
    pub problem_codes: Vec<String>,
    pub aggregate_cold_start_bytes: Option<u64>,
    pub reboot_capacity_bytes: Option<u64>,
    pub live_available_after_start_bytes: Option<u64>,
    pub disk_available_after_start_bytes: Option<u64>,
    pub policy: ResourcePolicy,
    pub snapshot: HostResourceSnapshot,
    pub selection: Option<AdmissionSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spark-agent", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct AdmissionSelection {
    pub engine_id: String,
    pub selection_kind: String,
    pub engine: String,
    pub image: String,
    pub fingerprint: String,
    pub artifacts: super::wire::ModelArtifactsDocument,
    pub artifact_fingerprint: String,
    pub compile_cache_namespace: String,
}

#[cfg(feature = "spark-agent")]
pub fn evaluate_admission(
    policy: &ResourcePolicy,
    snapshot: &HostResourceSnapshot,
    request: &AdmissionRequest,
    now_unix_ms: u64,
) -> AdmissionReport {
    let replaced_bytes = request
        .desired
        .iter()
        .find(|instance| instance.instance_name == request.candidate.instance_name)
        .map_or(0, |instance| instance.cold_start_peak_bytes);
    let aggregate = request
        .desired
        .iter()
        .filter(|instance| instance.instance_name != request.candidate.instance_name)
        .try_fold(request.candidate.cold_start_peak_bytes, |sum, instance| {
            sum.checked_add(instance.cold_start_peak_bytes)
        });
    let reboot_capacity = snapshot
        .mem_total_bytes
        .and_then(|total| total.checked_sub(policy.system_reserve_bytes));
    let live_after = snapshot.mem_available_bytes.and_then(|available| {
        available.checked_sub(
            request
                .candidate
                .incremental_start_peak_bytes
                .saturating_sub(replaced_bytes),
        )
    });
    let disk_after = snapshot
        .disk_available_bytes
        .and_then(|available| available.checked_sub(request.candidate.required_disk_bytes));
    let mut problems = Vec::new();
    if snapshot.observed_at_unix_ms > now_unix_ms
        || now_unix_ms.saturating_sub(snapshot.observed_at_unix_ms) > policy.max_snapshot_age_ms()
    {
        problems.push("spark.resources.telemetry-stale".into());
    }
    if !snapshot.is_complete() {
        problems.push("spark.resources.telemetry-missing".into());
    }
    if aggregate
        .zip(reboot_capacity)
        .is_none_or(|(used, cap)| used > cap)
        || live_after.is_none_or(|left| left < policy.system_reserve_bytes)
    {
        problems.push("spark.memory.admission-rejected".into());
    }
    if disk_after.is_none_or(|left| left < policy.disk_reserve_bytes) {
        problems.push("spark.disk.reserve".into());
    }
    if snapshot
        .memory_full_psi_avg10_percent
        .is_none_or(|psi| !psi.is_finite() || psi >= policy.memory_full_psi_avg10_percent)
        || snapshot.swap_in_pages_delta.is_none_or(|pages| pages > 0)
    {
        problems.push("spark.resources.pressure".into());
    }
    if !request.compatibility_verified {
        problems.push("spark.engine.unsupported".into());
    }
    if !request.guard_healthy {
        problems.push("spark.executor.guard-unhealthy".into());
    }
    AdmissionReport {
        schema: "sy.spark.admission-report/v1".into(),
        admitted: problems.is_empty(),
        problem_codes: problems,
        aggregate_cold_start_bytes: aggregate,
        reboot_capacity_bytes: reboot_capacity,
        live_available_after_start_bytes: live_after,
        disk_available_after_start_bytes: disk_after,
        policy: policy.clone(),
        snapshot: snapshot.clone(),
        selection: None,
    }
}

#[cfg(feature = "spark-agent")]
pub fn persistent_set_fits_reboot_envelope(
    policy: &ResourcePolicy,
    snapshot: &HostResourceSnapshot,
    desired: &[DeclaredEnvelope],
    now_unix_ms: u64,
) -> bool {
    let aggregate = desired.iter().try_fold(0_u64, |sum, instance| {
        sum.checked_add(instance.cold_start_peak_bytes)
    });
    snapshot.is_complete()
        && snapshot.observed_at_unix_ms <= now_unix_ms
        && now_unix_ms.saturating_sub(snapshot.observed_at_unix_ms) <= policy.max_snapshot_age_ms()
        && aggregate
            .zip(snapshot.mem_total_bytes)
            .is_some_and(|(used, total)| {
                total
                    .checked_sub(policy.system_reserve_bytes)
                    .is_some_and(|capacity| used <= capacity)
            })
        && snapshot
            .memory_full_psi_avg10_percent
            .is_some_and(|psi| psi.is_finite() && psi < policy.memory_full_psi_avg10_percent)
        && snapshot.swap_in_pages_delta == Some(0)
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionLeaseError {
    Busy { holder: String },
    Unavailable,
}

#[cfg(feature = "spark-agent")]
#[derive(Clone, Default)]
pub struct TransitionCoordinator {
    holder: Arc<Mutex<Option<String>>>,
}

#[cfg(feature = "spark-agent")]
impl TransitionCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_acquire(
        &self,
        operation_id: impl Into<String>,
    ) -> Result<TransitionLease, TransitionLeaseError> {
        let operation_id = operation_id.into();
        let mut holder = self
            .holder
            .lock()
            .map_err(|_| TransitionLeaseError::Unavailable)?;
        if let Some(holder) = holder.as_ref() {
            return Err(TransitionLeaseError::Busy {
                holder: holder.clone(),
            });
        }
        *holder = Some(operation_id.clone());
        Ok(TransitionLease {
            holder: Arc::clone(&self.holder),
            operation_id,
        })
    }
}

#[cfg(feature = "spark-agent")]
pub struct TransitionLease {
    holder: Arc<Mutex<Option<String>>>,
    operation_id: String,
}

#[cfg(feature = "spark-agent")]
impl std::fmt::Debug for TransitionLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransitionLease")
            .field("operation_id", &self.operation_id)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "spark-agent")]
impl TransitionLease {
    pub fn coordinator(&self) -> TransitionCoordinator {
        TransitionCoordinator {
            holder: Arc::clone(&self.holder),
        }
    }
}

#[cfg(feature = "spark-agent")]
impl Drop for TransitionLease {
    fn drop(&mut self) {
        if let Ok(mut holder) = self.holder.lock() {
            if holder.as_deref() == Some(&self.operation_id) {
                *holder = None;
            }
        }
    }
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnginePhase {
    Starting,
    Tuning,
    Healthy,
    Stopping,
    Failed,
}

#[cfg(feature = "spark-agent")]
impl EnginePhase {
    fn transitional(self) -> bool {
        matches!(self, Self::Starting | Self::Tuning)
    }
}

#[cfg(feature = "spark-agent")]
pub fn guard_interval_ms(policy: &ResourcePolicy, engines: &[ManagedEngine]) -> u64 {
    if engines.iter().any(|engine| engine.phase.transitional()) {
        policy.startup_guard_interval_ms
    } else {
        policy.steady_guard_interval_ms
    }
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedEngine {
    pub instance_id: String,
    pub generation: u64,
    pub phase: EnginePhase,
    pub started_sequence: u64,
    pub memory_bytes: u64,
    pub previous_memory_bytes: u64,
}

#[cfg(feature = "spark-agent")]
impl ManagedEngine {
    #[cfg(test)]
    fn new(
        instance_id: impl Into<String>,
        phase: EnginePhase,
        started_sequence: u64,
        memory_bytes: u64,
        previous_memory_bytes: u64,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            generation: 1,
            phase,
            started_sequence,
            memory_bytes,
            previous_memory_bytes,
        }
    }
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmergencyDecision {
    pub schema: String,
    pub instance_id: String,
    pub generation: u64,
    pub cause: String,
    pub mem_available_bytes: u64,
    pub memory_full_psi_avg10_percent: f64,
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmergencyRecord {
    pub schema: String,
    pub event_id: String,
    pub occurred_at_unix_ms: u64,
    pub decision: EmergencyDecision,
}

#[cfg(feature = "spark-agent")]
impl EmergencyRecord {
    pub fn from_decision(decision: EmergencyDecision) -> Self {
        Self {
            schema: "sy.spark.emergency-record/v1".into(),
            event_id: ulid::Ulid::new().to_string(),
            occurred_at_unix_ms: unix_millis(),
            decision,
        }
    }
}

#[cfg(feature = "spark-agent")]
pub fn append_emergency_record(path: &Path, record: &EmergencyRecord) -> Result<(), SampleError> {
    let mut line = serde_json::to_vec(record).map_err(|_| SampleError::Invalid)?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| SampleError::Unavailable)?;
    let metadata = file.metadata().map_err(|_| SampleError::Unavailable)?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(SampleError::Invalid);
    }
    file.write_all(&line)
        .and_then(|()| file.sync_data())
        .map_err(|_| SampleError::Unavailable)?;
    std::fs::File::open(path.parent().ok_or(SampleError::Invalid)?)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| SampleError::Unavailable)
}

#[cfg(feature = "spark-agent")]
pub fn read_emergency_records(path: &Path) -> Result<Vec<EmergencyRecord>, SampleError> {
    const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(SampleError::Invalid),
    };
    let metadata = file.metadata().map_err(|_| SampleError::Unavailable)?;
    if !metadata.file_type().is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.len() > MAX_JOURNAL_BYTES
    {
        return Err(SampleError::Invalid);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| SampleError::Unavailable)?;
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).map_err(|_| SampleError::Invalid))
        .collect()
}

#[cfg(feature = "spark-agent")]
pub struct EmergencyGuard {
    policy: ResourcePolicy,
    consecutive_floor_breaches: u32,
    suppressed: BTreeSet<(String, u64)>,
}

#[cfg(feature = "spark-agent")]
impl EmergencyGuard {
    pub fn new(policy: ResourcePolicy) -> Self {
        Self {
            policy,
            consecutive_floor_breaches: 0,
            suppressed: BTreeSet::new(),
        }
    }

    pub fn observe(
        &mut self,
        snapshot: &HostResourceSnapshot,
        engines: &[ManagedEngine],
    ) -> Option<EmergencyDecision> {
        let available = snapshot.mem_available_bytes?;
        let psi = snapshot.memory_full_psi_avg10_percent?;
        if available < self.policy.emergency_available_floor_bytes {
            self.consecutive_floor_breaches = self.consecutive_floor_breaches.saturating_add(1);
        } else {
            self.consecutive_floor_breaches = 0;
        }
        let psi_breach = psi >= self.policy.memory_full_psi_avg10_percent;
        if !psi_breach
            && self.consecutive_floor_breaches < self.policy.emergency_consecutive_samples
        {
            return None;
        }
        let victim = select_victim(engines, &self.suppressed)?;
        Some(EmergencyDecision {
            schema: "sy.spark.emergency-decision/v1".into(),
            instance_id: victim.instance_id.clone(),
            generation: victim.generation,
            cause: if psi_breach {
                "memory-full-psi".into()
            } else {
                "memory-available-floor".into()
            },
            mem_available_bytes: available,
            memory_full_psi_avg10_percent: psi,
        })
    }

    pub fn suppress(&mut self, decision: &EmergencyDecision) {
        self.suppressed
            .insert((decision.instance_id.clone(), decision.generation));
    }
}

#[cfg(feature = "spark-agent")]
fn select_victim<'a>(
    engines: &'a [ManagedEngine],
    suppressed: &BTreeSet<(String, u64)>,
) -> Option<&'a ManagedEngine> {
    engines
        .iter()
        .filter(|engine| !suppressed.contains(&(engine.instance_id.clone(), engine.generation)))
        .filter(|engine| engine.phase.transitional())
        .max_by_key(|engine| (engine.started_sequence, &engine.instance_id))
        .or_else(|| {
            engines
                .iter()
                .filter(|engine| {
                    !suppressed.contains(&(engine.instance_id.clone(), engine.generation))
                })
                .filter(|engine| {
                    engine.phase == EnginePhase::Healthy
                        && engine.memory_bytes > engine.previous_memory_bytes
                })
                .max_by_key(|engine| (engine.started_sequence, &engine.instance_id))
        })
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedCgroupIdentity {
    pub managed_label: bool,
    pub engine_role: String,
    pub instance_id: String,
    pub generation: u64,
    pub container_id: String,
    pub init_pid: u32,
    pub pid_start_time_ticks: u64,
    pub cgroup_path: String,
}

#[cfg(feature = "spark-agent")]
impl ManagedCgroupIdentity {
    #[cfg(test)]
    fn new(container_id: &str, cgroup_path: &str, init_pid: u32, start_time: u64) -> Self {
        Self {
            managed_label: true,
            engine_role: "engine".into(),
            instance_id: "test-instance".into(),
            generation: 1,
            container_id: container_id.into(),
            init_pid,
            pid_start_time_ticks: start_time,
            cgroup_path: cgroup_path.into(),
        }
    }
}

#[cfg(feature = "spark-agent")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupKillError {
    IdentityMismatch,
    UnsafePath,
    Io,
}

#[cfg(feature = "spark-agent")]
pub fn kill_managed_cgroup(
    cgroup_root: &Path,
    expected: &ManagedCgroupIdentity,
    observed: &ManagedCgroupIdentity,
) -> Result<(), CgroupKillError> {
    if expected != observed
        || !expected.managed_label
        || expected.engine_role != "engine"
        || expected.container_id.len() != 64
        || !expected
            .container_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CgroupKillError::IdentityMismatch);
    }
    let relative = Path::new(&expected.cgroup_path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || relative.parent() != Some(Path::new("system.slice"))
        || relative.file_name().and_then(|value| value.to_str())
            != Some(&format!("docker-{}.scope", expected.container_id))
    {
        return Err(CgroupKillError::UnsafePath);
    }
    let root = std::fs::canonicalize(cgroup_root).map_err(|_| CgroupKillError::Io)?;
    let directory = std::fs::canonicalize(root.join(relative)).map_err(|_| CgroupKillError::Io)?;
    if !directory.starts_with(&root) {
        return Err(CgroupKillError::UnsafePath);
    }
    let mut kill = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join("cgroup.kill"))
        .map_err(|_| CgroupKillError::Io)?;
    kill.write_all(b"1").map_err(|_| CgroupKillError::Io)
}

#[cfg(all(test, feature = "spark-agent"))]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const NOW_MS: u64 = 1_000_000;

    #[test]
    fn aggregate_reboot_and_live_envelopes_hold_at_boundaries() {
        let policy = ResourcePolicy::capacity_first();
        let snapshot = HostResourceSnapshot::complete(NOW_MS, 64 * GIB, 40 * GIB, 200 * GIB);
        let request = AdmissionRequest {
            desired: vec![DeclaredEnvelope::new("existing", 24 * GIB)],
            candidate: CandidateEnvelope::new("candidate", 32 * GIB, 32 * GIB, GIB),
            compatibility_verified: true,
            guard_healthy: true,
        };

        let report = evaluate_admission(&policy, &snapshot, &request, NOW_MS);

        assert!(report.admitted);
    }

    #[test]
    fn same_named_desired_instance_is_replaced_in_admission_accounting() {
        let policy = ResourcePolicy::capacity_first();
        let snapshot = HostResourceSnapshot::complete(NOW_MS, 128 * GIB, 44 * GIB, 200 * GIB);
        let request = AdmissionRequest {
            desired: vec![DeclaredEnvelope::new("ornith", 64 * GIB)],
            candidate: CandidateEnvelope::new("ornith", 64 * GIB, 64 * GIB, GIB),
            compatibility_verified: true,
            guard_healthy: true,
        };

        let report = evaluate_admission(&policy, &snapshot, &request, NOW_MS);
        assert_eq!(
            (
                report.admitted,
                report.aggregate_cold_start_bytes,
                report.live_available_after_start_bytes,
            ),
            (true, Some(64 * GIB), Some(44 * GIB))
        );
    }

    #[test]
    fn differently_named_candidate_remains_additive_in_admission_accounting() {
        let policy = ResourcePolicy::capacity_first();
        let snapshot = HostResourceSnapshot::complete(NOW_MS, 128 * GIB, 80 * GIB, 200 * GIB);
        let request = AdmissionRequest {
            desired: vec![DeclaredEnvelope::new("ornith", 64 * GIB)],
            candidate: CandidateEnvelope::new("ornith-copy", 64 * GIB, 64 * GIB, GIB),
            compatibility_verified: true,
            guard_healthy: true,
        };

        let report = evaluate_admission(&policy, &snapshot, &request, NOW_MS);
        assert_eq!(
            (report.admitted, report.aggregate_cold_start_bytes),
            (false, Some(128 * GIB))
        );
    }

    #[test]
    fn aggregate_reboot_and_live_envelopes_reject_overflow_and_one_byte_short() {
        let policy = ResourcePolicy::capacity_first();
        let mut snapshot =
            HostResourceSnapshot::complete(NOW_MS, 64 * GIB, 40 * GIB - 1, 200 * GIB);
        let mut request = AdmissionRequest {
            desired: vec![DeclaredEnvelope::new("existing", 24 * GIB)],
            candidate: CandidateEnvelope::new("candidate", 32 * GIB, 32 * GIB, 1),
            compatibility_verified: true,
            guard_healthy: true,
        };
        assert!(!evaluate_admission(&policy, &snapshot, &request, NOW_MS).admitted);
        snapshot.mem_available_bytes = Some(u64::MAX);
        snapshot.mem_total_bytes = Some(u64::MAX);
        request.desired[0].cold_start_peak_bytes = u64::MAX;
        assert!(!evaluate_admission(&policy, &snapshot, &request, NOW_MS).admitted);
    }

    #[test]
    fn persistent_restart_set_must_fit_aggregate_reboot_envelope() {
        let policy = ResourcePolicy::capacity_first();
        let snapshot = HostResourceSnapshot::complete(NOW_MS, 64 * GIB, 40 * GIB, 200 * GIB);
        let desired = vec![
            DeclaredEnvelope::new("one", 30 * GIB),
            DeclaredEnvelope::new("two", 30 * GIB),
        ];
        assert!(!persistent_set_fits_reboot_envelope(
            &policy, &snapshot, &desired, NOW_MS
        ));
    }

    #[test]
    fn missing_stale_psi_or_swap_telemetry_fails_closed() {
        let policy = ResourcePolicy::capacity_first();
        let request = AdmissionRequest {
            desired: Vec::new(),
            candidate: CandidateEnvelope::new("candidate", GIB, GIB, GIB),
            compatibility_verified: true,
            guard_healthy: true,
        };
        let mut snapshot = HostResourceSnapshot::complete(
            NOW_MS - policy.max_snapshot_age_ms() - 1,
            64 * GIB,
            40 * GIB,
            200 * GIB,
        );
        assert!(!evaluate_admission(&policy, &snapshot, &request, NOW_MS).admitted);
        snapshot.observed_at_unix_ms = NOW_MS;
        snapshot.memory_full_psi_avg10_percent = None;
        assert!(!evaluate_admission(&policy, &snapshot, &request, NOW_MS).admitted);
        snapshot.memory_full_psi_avg10_percent = Some(0.0);
        snapshot.swap_in_pages_delta = None;
        assert!(!evaluate_admission(&policy, &snapshot, &request, NOW_MS).admitted);
    }

    #[test]
    fn procfs_sampler_requires_two_samples_and_reports_swap_delta() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("meminfo"),
            "MemTotal: 65536 kB\nMemAvailable: 32768 kB\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("pressure"),
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=1\nfull avg10=0.25 avg60=0.00 avg300=0.00 total=1\n",
        )
        .unwrap();
        std::fs::write(root.path().join("vmstat"), "pswpin 10\n").unwrap();
        let mut sampler = ProcfsHostSampler::with_paths(root.path());
        assert_eq!(sampler.sample().unwrap().swap_in_pages_delta, None);
        std::fs::write(root.path().join("vmstat"), "pswpin 12\n").unwrap();
        let snapshot = sampler.sample().unwrap();
        assert_eq!(snapshot.swap_in_pages_delta, Some(2));
    }

    #[test]
    fn declarative_policy_satisfies_safety_invariants() {
        #[derive(Deserialize)]
        struct AgentPolicy {
            resources: ResourcePolicyConfig,
        }
        let configured: AgentPolicy =
            toml::from_str(include_str!("../../configs/sy/spark/agent.toml")).unwrap();
        let policy = configured.resources.policy().unwrap();
        assert!(policy.system_reserve_bytes > 0);
        assert!(policy.emergency_available_floor_bytes > 0);
        assert!(policy.disk_reserve_bytes > 0);
        assert!(policy.emergency_consecutive_samples > 0);
        assert_eq!(policy.memory_full_psi_avg10_percent, 100.0);
    }

    #[tokio::test]
    async fn only_one_high_memory_transition_can_hold_lease() {
        let coordinator = TransitionCoordinator::new();
        let first = coordinator.try_acquire("first").expect("first lease");
        assert_eq!(
            coordinator.try_acquire("second").unwrap_err(),
            TransitionLeaseError::Busy {
                holder: "first".into()
            }
        );
        drop(first);
        assert!(coordinator.try_acquire("second").is_ok());
    }

    #[test]
    fn guard_orders_transitional_then_recent_growing_victim() {
        let policy = ResourcePolicy::capacity_first();
        let engines = vec![
            ManagedEngine::new("healthy-new", EnginePhase::Healthy, 30, 12, 10),
            ManagedEngine::new("starting-old", EnginePhase::Starting, 10, 4, 1),
            ManagedEngine::new("tuning-new", EnginePhase::Tuning, 20, 3, 1),
        ];
        let mut guard = EmergencyGuard::new(policy.clone());
        let low = HostResourceSnapshot::complete(NOW_MS, 64 * GIB, 7 * GIB, 200 * GIB);
        assert!(guard.observe(&low, &engines).is_none());
        assert!(guard.observe(&low, &engines).is_none());
        assert_eq!(
            guard.observe(&low, &engines).unwrap().instance_id,
            "tuning-new"
        );

        let healthy_only = vec![
            ManagedEngine::new("healthy-new", EnginePhase::Healthy, 30, 12, 10),
            ManagedEngine::new("healthy-flat", EnginePhase::Healthy, 40, 8, 8),
        ];
        let mut guard = EmergencyGuard::new(policy);
        guard.observe(&low, &healthy_only);
        guard.observe(&low, &healthy_only);
        assert_eq!(
            guard.observe(&low, &healthy_only).unwrap().instance_id,
            "healthy-new"
        );
    }

    #[test]
    fn guard_sampling_uses_startup_interval_only_during_high_memory_transition() {
        let policy = ResourcePolicy::capacity_first();
        let healthy = ManagedEngine::new("healthy", EnginePhase::Healthy, 1, 1, 1);
        let starting = ManagedEngine::new("starting", EnginePhase::Starting, 2, 1, 0);
        assert_eq!(guard_interval_ms(&policy, &[healthy]), 2_000);
        assert_eq!(guard_interval_ms(&policy, &[starting]), 500);
    }

    #[test]
    fn cgroup_kill_requires_label_pid_start_time_and_path_match() {
        let root = tempfile::tempdir().unwrap();
        let container_id = "a".repeat(64);
        let relative = format!("system.slice/docker-{container_id}.scope");
        let directory = root.path().join(&relative);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("cgroup.kill"), b"0").unwrap();
        let expected = ManagedCgroupIdentity::new(&container_id, &relative, 123, 456);
        let mut observed = expected.clone();
        observed.pid_start_time_ticks += 1;
        assert_eq!(
            kill_managed_cgroup(root.path(), &expected, &observed).unwrap_err(),
            CgroupKillError::IdentityMismatch
        );
        assert_eq!(std::fs::read(directory.join("cgroup.kill")).unwrap(), b"0");
        kill_managed_cgroup(root.path(), &expected, &expected).unwrap();
        assert_eq!(std::fs::read(directory.join("cgroup.kill")).unwrap(), b"1");
    }
}
