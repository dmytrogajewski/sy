use std::{collections::BTreeMap, env, path::PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::agt::{protocol::exit, AgtError};

const DEFAULT_AGENTS_TOML: &str = include_str!("../../configs/sy/agents.toml");

#[derive(Deserialize, Clone, Debug)]
pub struct AgentSpec {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_version_args")]
    pub version_args: Vec<String>,
    /// Sandbox profile name (matches `configs/policy/profiles/<name>.toml`)
    /// applied via `systemd-run --user --scope` + `sy agt sandbox-exec`
    /// when the daemon spawns the ACP child. `None` disables sandboxing
    /// for that agent — primarily for the in-tree daemon tests that
    /// spawn `cat` as a synthetic stdio stand-in for ACP.
    #[serde(default = "default_sandbox_profile")]
    pub sandbox_profile: Option<String>,
}

fn default_version_args() -> Vec<String> {
    vec!["--version".into()]
}

/// Default sandbox profile applied when `agents.toml` omits the field.
/// SPEC §4.4 step 4: every ACP child runs inside the cgroup + Landlock +
/// seccomp envelope of the `normal` profile by default; operators can
/// override per-agent in `~/.config/sy/agents.toml`.
fn default_sandbox_profile() -> Option<String> {
    Some("normal".into())
}

#[derive(Deserialize)]
struct File {
    #[serde(default, rename = "agent")]
    agents: Vec<AgentSpec>,
}

pub fn load() -> Result<Vec<AgentSpec>> {
    let path = path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => DEFAULT_AGENTS_TOML.to_string(),
    };
    let f: File = toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    if f.agents.is_empty() {
        return Err(AgtError {
            code: exit::REGISTRY,
            msg: format!(
                "agent registry empty (edit {} to add an [[agent]] block)",
                path.display()
            ),
        }
        .into());
    }
    Ok(f.agents)
}

pub fn find(name: &str) -> Result<AgentSpec> {
    load()?.into_iter().find(|a| a.name == name).ok_or_else(|| {
        AgtError {
            code: exit::REGISTRY,
            msg: format!("agent not found in registry: {name}"),
        }
        .into()
    })
}

fn path() -> PathBuf {
    if let Ok(x) = env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("sy/agents.toml");
        }
    }
    let home = env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/sy/agents.toml")
}
