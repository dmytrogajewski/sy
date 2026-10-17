use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

pub fn socket_path_for(plane: &str) -> Result<PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .with_context(|| format!("read XDG_RUNTIME_DIR for {plane} metrics socket"))?;
    if base.is_empty() {
        return Err(anyhow!(
            "XDG_RUNTIME_DIR is empty; refusing to bind {plane} metrics socket"
        ));
    }
    Ok(PathBuf::from(base)
        .join("sy")
        .join(plane)
        .join("metrics.sock"))
}
