//! `[[stack.onto]]` integrations — hand a stack item to a configured app.
//!
//! Reads sy.toml from the repo root (same lookup as `sy apply`). Each
//! entry is `{ name, template }` where `{file}` in the template is replaced
//! by `state::link_path(item)` (file path for files, materialised temp file
//! for content items). The command is shell-tokenised lightly: split on
//! whitespace, then interpolate.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::state;

#[derive(Debug, Clone, Deserialize)]
pub struct OntoEntry {
    pub name: String,
    pub template: String,
}

#[derive(Debug, Default, Deserialize)]
struct StackSection {
    #[serde(default)]
    onto: Vec<OntoEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct SyFile {
    #[serde(default)]
    stack: StackSection,
}

fn load_entries() -> Result<Vec<OntoEntry>> {
    let root = find_root()?;
    let p = root.join("sy.toml");
    if !p.exists() {
        return Ok(Vec::new());
    }
    let s = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let f: SyFile = toml::from_str(&s).with_context(|| format!("parse {}", p.display()))?;
    Ok(f.stack.onto)
}

pub fn list_names() -> Result<Vec<String>> {
    Ok(load_entries()?.into_iter().map(|e| e.name).collect())
}

pub fn run(integration: &str, id: &str) -> Result<()> {
    let entries = load_entries()?;
    let entry = entries
        .iter()
        .find(|e| e.name == integration)
        .ok_or_else(|| anyhow::anyhow!("unknown onto integration: {integration}"))?;

    let items = state::load()?;
    let it = state::find(&items, id)?;
    let path = state::link_path(it)?;

    // Templates are evaluated via `sh -c`: the user writes a shell command
    // string and `{file}` is substituted with the resolved path. This keeps
    // quoting / pipes / redirections natural.
    let cmd = entry.template.replace("{file}", &shell_quote(&path));
    let status = Command::new("sh")
        .args(["-c", &cmd])
        .status()
        .with_context(|| format!("spawn sh -c for {integration}"))?;
    if !status.success() {
        return Err(super::StackError {
            code: super::exit::INTEGRATION_FAILED,
            msg: format!(
                "onto {integration} exited with {}",
                status.code().unwrap_or(-1)
            ),
        }
        .into());
    }
    Ok(())
}

/// Single-quote a path safely for inclusion inside a `sh -c` command line.
fn shell_quote(p: &Path) -> String {
    let s = p.display().to_string();
    // Wrap in single quotes; escape any embedded single quote by closing,
    // injecting an escaped quote, and reopening.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Walk up from cwd looking for sy.toml; mirrors `find_root` in main.rs but
/// scoped narrowly so we don't need to re-export anything.
fn find_root() -> Result<PathBuf> {
    if let Ok(r) = env::var("SY_ROOT") {
        if !r.is_empty() {
            return Ok(PathBuf::from(r));
        }
    }
    let mut cur = env::current_dir().context("cwd")?;
    loop {
        if cur.join("sy.toml").exists()
            && cur.join("configs").is_dir()
            && cur.join("themes").is_dir()
        {
            return Ok(cur);
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => {
                // Fall back to ~/sources/sy if nothing found — this lets
                // `sy stack onto` work from any cwd. If the fallback is
                // wrong, return an empty integration list rather than fail.
                if let Ok(home) = env::var("HOME") {
                    let guess = PathBuf::from(home).join("sources/sy");
                    if guess.exists() {
                        return Ok(guess);
                    }
                }
                return Err(anyhow::anyhow!(
                    "could not find sy repo root (set SY_ROOT or run from inside the repo)"
                ));
            }
        }
    }
}
