//! Resolve config sources and destinations for a checkout or RPM install.

use anyhow::{Context, Result, anyhow};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Component, Path, PathBuf},
};

pub fn find_root() -> Result<PathBuf> {
    find_root_from(
        env::current_dir().context("cwd")?.as_path(),
        Path::new("/usr/share/sy"),
    )
}

fn find_root_from(start: &Path, packaged: &Path) -> Result<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join("configs").is_dir() && cur.join("themes").is_dir() {
            return Ok(cur);
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None if packaged.join("configs").is_dir() && packaged.join("themes").is_dir() => {
                return Ok(packaged.to_path_buf());
            }
            None => {
                return Err(anyhow!(
                    "reached filesystem root without finding configs/ + themes/"
                ));
            }
        }
    }
}

pub fn default_target() -> Result<PathBuf> {
    if let Ok(x) = env::var("XDG_CONFIG_HOME")
        && !x.is_empty()
    {
        return Ok(PathBuf::from(x));
    }
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config"))
}

pub fn is_packaged_root(root: &Path) -> bool {
    root == Path::new("/usr/share/sy")
}

pub fn should_prepare_agent_bridge(root: &Path, feature_binary: &Path) -> bool {
    !is_packaged_root(root) || feature_binary.is_file()
}

/// Records files rendered by `sy apply` so removed feature RPMs can be
/// reconciled without deleting files edited after the previous apply.
pub struct AppliedConfigs {
    target: PathBuf,
    current: BTreeMap<String, String>,
}

impl AppliedConfigs {
    pub fn new(target: &Path) -> Self {
        Self {
            target: target.to_path_buf(),
            current: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, relative: &Path, rendered: &[u8]) -> Result<()> {
        let name = relative
            .to_str()
            .with_context(|| format!("config path is not UTF-8: {}", relative.display()))?;
        self.current
            .insert(name.to_owned(), blake3::hash(rendered).to_hex().to_string());
        Ok(())
    }

    pub fn finish(self, dry: bool) -> Result<bool> {
        let manifest = self.target.join(".sy-applied-configs.json");
        let previous: BTreeMap<String, String> = match fs::read(&manifest) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", manifest.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", manifest.display())),
        };
        let mut waybar_removed = false;
        for (name, digest) in previous {
            if self.current.contains_key(&name) {
                continue;
            }
            let relative = Path::new(&name);
            if !relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            {
                continue;
            }
            let dest = self.target.join(relative);
            let mut path = self.target.clone();
            let mut linked = false;
            for component in relative.components() {
                path.push(component);
                if fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    linked = true;
                    break;
                }
            }
            if linked {
                continue;
            }
            let Ok(bytes) = fs::read(&dest) else {
                continue;
            };
            if blake3::hash(&bytes).to_hex().as_str() != digest {
                continue;
            }
            println!("  - {}", relative.display());
            waybar_removed |= relative.starts_with("waybar");
            if !dry {
                fs::remove_file(&dest).with_context(|| format!("remove {}", dest.display()))?;
            }
        }
        if !dry {
            fs::create_dir_all(&self.target)?;
            let temporary = self
                .target
                .join(format!(".sy-applied-configs-{}.tmp", std::process::id()));
            fs::write(&temporary, serde_json::to_vec_pretty(&self.current)?)?;
            fs::rename(&temporary, &manifest)
                .with_context(|| format!("replace {}", manifest.display()))?;
        }
        Ok(waybar_removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_tree_is_used_outside_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let packaged = temp.path().join("share/sy");
        std::fs::create_dir_all(packaged.join("configs")).unwrap();
        std::fs::create_dir_all(packaged.join("themes")).unwrap();
        assert_eq!(
            find_root_from(Path::new("/tmp"), &packaged).unwrap(),
            packaged
        );
    }

    #[test]
    fn packaged_root_uses_the_system_binary() {
        assert!(is_packaged_root(Path::new("/usr/share/sy")));
        assert!(!is_packaged_root(Path::new("/tmp/sy")));
    }

    #[test]
    fn packaged_desktop_skips_agent_setup_until_feature_is_installed() {
        let temp = tempfile::tempdir().unwrap();
        let feature = temp.path().join("sy-system-agent");
        assert!(!should_prepare_agent_bridge(
            Path::new("/usr/share/sy"),
            &feature
        ));
        std::fs::write(&feature, "").unwrap();
        assert!(should_prepare_agent_bridge(
            Path::new("/usr/share/sy"),
            &feature
        ));
        assert!(should_prepare_agent_bridge(temp.path(), &feature));
    }

    #[test]
    fn stale_applied_configs_are_removed_only_if_unmodified() {
        let target = tempfile::tempdir().unwrap();
        let old = target.path().join("waybar/old.jsonc");
        let edited = target.path().join("sy/edited.toml");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::create_dir_all(edited.parent().unwrap()).unwrap();
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&edited, "original").unwrap();
        let mut first = AppliedConfigs::new(target.path());
        first.record(Path::new("waybar/old.jsonc"), b"old").unwrap();
        first
            .record(Path::new("sy/edited.toml"), b"original")
            .unwrap();
        first.finish(false).unwrap();
        std::fs::write(&edited, "user edit").unwrap();
        assert!(AppliedConfigs::new(target.path()).finish(true).unwrap());
        assert!(old.exists());
        assert!(AppliedConfigs::new(target.path()).finish(false).unwrap());
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(edited).unwrap(), "user edit");
    }

    #[test]
    fn manifest_cannot_remove_files_outside_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config");
        fs::create_dir_all(&target).unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        let digest = blake3::hash(b"keep").to_hex().to_string();
        fs::write(
            target.join(".sy-applied-configs.json"),
            serde_json::to_vec(&BTreeMap::from([("../outside", digest)])).unwrap(),
        )
        .unwrap();
        AppliedConfigs::new(&target).finish(false).unwrap();
        assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn manifest_does_not_follow_symlinked_parent() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("old.jsonc"), "keep").unwrap();
        std::os::unix::fs::symlink(&outside, target.join("waybar")).unwrap();
        let digest = blake3::hash(b"keep").to_hex().to_string();
        fs::write(
            target.join(".sy-applied-configs.json"),
            serde_json::to_vec(&BTreeMap::from([("waybar/old.jsonc", digest)])).unwrap(),
        )
        .unwrap();
        assert!(!AppliedConfigs::new(&target).finish(false).unwrap());
        assert_eq!(
            fs::read_to_string(outside.join("old.jsonc")).unwrap(),
            "keep"
        );
    }
}
