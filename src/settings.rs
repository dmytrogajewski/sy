//! Declarative workstation settings and independently pinned integrations.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{fs, path::Path};

#[derive(Deserialize, Default)]
pub struct SyFile {
    pub theme: Option<String>,
    #[serde(default)]
    pub integrations: crate::sparkplane_bridge::Integrations,
}

pub fn load_sy_file(root: &Path) -> Result<SyFile> {
    let path = root.join("sy.toml");
    if !path.exists() {
        return Ok(SyFile::default());
    }
    let source = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&source).with_context(|| format!("parse {}", path.display()))
}
