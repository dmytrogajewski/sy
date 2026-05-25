//! `sy list-themes` helper: scan the rice's `themes/` directory and
//! print each `.toml` palette name (without extension), one per line,
//! sorted. Extracted from `src/main.rs` to keep that file under the
//! `scripts/check_main_rs_loc.sh` budget.

use std::{fs, path::Path};

use anyhow::Result;

pub fn list(root: &Path) -> Result<()> {
    let dir = root.join("themes");
    if !dir.is_dir() {
        return Ok(());
    }
    let mut names: Vec<String> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|s| s.to_str()) == Some("toml"))
                .then(|| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                })
                .flatten()
        })
        .collect();
    names.sort();
    for n in names {
        println!("{n}");
    }
    Ok(())
}
