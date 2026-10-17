//! Runtime paths shared by the aiplane CLI, daemon, and workers.

use std::{env, path::PathBuf};

use anyhow::{Context, Result};
use sy_core::workload::WorkloadKind;

pub fn knowledge_socket_path() -> PathBuf {
    sy_ipc::paths::for_endpoint("knowledge").expect("known IPC endpoint")
}

pub fn aiplane_socket_path() -> PathBuf {
    sy_ipc::paths::for_endpoint("aiplane").expect("known IPC endpoint")
}

pub fn worker_socket_path(kind: WorkloadKind) -> PathBuf {
    let runtime = aiplane_socket_path();
    runtime
        .parent()
        .expect("aiplane socket has a runtime directory")
        .join(format!("sy-aiplane-worker-{}.sock", kind.as_str()))
}

pub fn dedicated_installed() -> bool {
    env::var_os("SY_AIPLANE_SOCKET").is_some()
        || std::path::Path::new("/usr/libexec/sy/sy-system-aiplane").is_file()
}

pub fn inference_socket_path() -> PathBuf {
    if let Some(path) = env::var_os("SY_AIPLANE_SOCKET") {
        return PathBuf::from(path);
    }
    let standalone = aiplane_socket_path();
    if dedicated_installed() || standalone.exists() {
        standalone
    } else {
        knowledge_socket_path()
    }
}

pub fn dedicated_selected() -> bool {
    dedicated_installed() || aiplane_socket_path().exists()
}

/// Model and compiler caches, overridable for tests and separate installations.
pub fn cache_root() -> PathBuf {
    if let Some(v) = env::var_os("SY_AIPLANE_CACHE_DIR") {
        return PathBuf::from(v);
    }
    let home = env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/sy/aiplane")
}

/// AI status directory. The caller creates it when writing a snapshot.
pub fn state_root() -> Result<PathBuf> {
    let base = match env::var("XDG_STATE_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env::var("HOME").context("HOME not set")?).join(".local/state"),
    };
    Ok(base.join("sy").join("aiplane"))
}
