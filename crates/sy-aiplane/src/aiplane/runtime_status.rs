//! AI daemon health snapshot, separate from the knowledge index state.

use std::{collections::HashMap, fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{
    ipc,
    registry::{WorkloadHealth, WorkloadKind, WorkloadState},
    status,
    supervisor::Supervisor,
};

const FRESH_SECS: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub ts_unix: u64,
    pub daemon_running: bool,
    pub embed_backend: String,
    pub workloads: HashMap<String, WorkloadHealth>,
    pub warm_models: Vec<String>,
    pub queue_depths: HashMap<String, usize>,
    pub inflight: usize,
}

impl RuntimeStatus {
    pub fn capture(supervisor: &Supervisor, daemon_running: bool) -> Self {
        let workloads: HashMap<String, WorkloadHealth> = supervisor
            .all_health()
            .into_iter()
            .map(|(kind, health)| {
                let health = match health {
                    Some(health) => WorkloadHealth {
                        loaded: health.state.is_ready(),
                        last_call_unix: health.ready_at_unix,
                        ema_ms: health.ema_ms,
                        calls: health.calls,
                        errors: health.errors,
                        backend: match &health.state {
                            WorkloadState::Ready { backend } => backend.clone(),
                            _ => String::new(),
                        },
                        state: health.state,
                    },
                    None => WorkloadHealth {
                        state: WorkloadState::Loading,
                        ..Default::default()
                    },
                };
                (kind.as_str().to_owned(), health)
            })
            .collect();
        let embed_backend = workloads
            .get(WorkloadKind::Embed.as_str())
            .map(|health| match &health.state {
                WorkloadState::Ready { backend } => backend.as_str(),
                WorkloadState::Loading => "loading",
                WorkloadState::Failed { .. } => "failed",
                WorkloadState::NotPrepared => "not-prepared",
                WorkloadState::Unavailable => "unavailable",
            })
            .unwrap_or("unloaded")
            .to_owned();
        let inflight = supervisor
            .all_health()
            .into_values()
            .filter(|health| {
                health
                    .as_ref()
                    .and_then(|health| health.inflight_request_id.as_ref())
                    .is_some()
            })
            .count();
        let queue_depths = ipc::current_scheduler()
            .map(|scheduler| {
                scheduler
                    .queue_depths()
                    .into_iter()
                    .map(|(priority, depth)| (priority.as_str().to_owned(), depth))
                    .collect()
            })
            .unwrap_or_else(status::default_queue_depths);
        Self {
            ts_unix: status::now_secs(),
            daemon_running,
            embed_backend,
            workloads,
            warm_models: supervisor.warm_models(),
            queue_depths,
            inflight,
        }
    }
}

fn path() -> Result<std::path::PathBuf> {
    Ok(status::root_dir()?.join("runtime.json"))
}

pub fn save(snapshot: &RuntimeStatus) -> Result<()> {
    let path = path()?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(snapshot)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub fn load_fresh() -> Result<RuntimeStatus> {
    let path = path()?;
    let snapshot = load_at(&path)?;
    anyhow::ensure!(
        snapshot.daemon_running
            && status::now_secs().saturating_sub(snapshot.ts_unix) <= FRESH_SECS,
        "aiplane runtime status is stale or stopped"
    );
    Ok(snapshot)
}

fn load_at(path: &Path) -> Result<RuntimeStatus> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_supervisor_has_an_unloaded_runtime_snapshot() {
        let supervisor = Supervisor::new();
        let snapshot = RuntimeStatus::capture(&supervisor, true);
        assert_eq!(snapshot.embed_backend, "unloaded");
        assert!(snapshot.workloads.is_empty());
        assert_eq!(snapshot.inflight, 0);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.json");
        fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(load_at(&path).unwrap().embed_backend, "unloaded");
    }
}
