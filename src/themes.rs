//! `sy list-themes` helper: scan the rice's `themes/` directory and
//! print each `.toml` palette name (without extension), one per line,
//! sorted. Extracted from `src/main.rs` to keep that file under the
//! `scripts/check_main_rs_loc.sh` budget.

use std::{env, fs, path::Path};

use anyhow::{Context, Result};

use crate::desktop_paths;

pub fn load(root: &Path, name: &str) -> Result<toml::Table> {
    load_with_feature_dir(
        root,
        name,
        desktop_paths::is_packaged_root(root),
        Path::new("/usr/libexec/sy"),
    )
}

fn load_with_feature_dir(
    root: &Path,
    name: &str,
    packaged: bool,
    feature_dir: &Path,
) -> Result<toml::Table> {
    let path = root.join("themes").join(format!("{name}.toml"));
    let source =
        fs::read_to_string(&path).with_context(|| format!("theme file {}", path.display()))?;
    let mut ctx: toml::Table =
        toml::from_str(&source).with_context(|| format!("parse {}", path.display()))?;
    if !ctx.contains_key("home")
        && let Ok(home) = env::var("HOME")
    {
        ctx.insert("home".into(), toml::Value::String(home));
    }
    let sy_bin = if packaged {
        "/usr/bin/sy".to_string()
    } else {
        format!(
            "{}/.local/bin/sy",
            env::var("HOME").context("HOME not set")?
        )
    };
    ctx.insert("sy_bin".into(), toml::Value::String(sy_bin));
    let foot_bin = if packaged {
        "/usr/bin/foot".to_owned()
    } else {
        format!(
            "{}/.local/bin/foot",
            env::var("HOME").context("HOME not set")?
        )
    };
    ctx.insert("foot_bin".into(), toml::Value::String(foot_bin));
    let mut features = toml::Table::new();
    for (name, bin) in [
        ("power", "sy-system-power"),
        ("agent", "sy-system-agent"),
        ("aiplane", "sy-system-aiplane"),
        ("auth", "sy-system-auth"),
        ("filemanager", "sy-filemanager"),
        ("knowledge", "sy-system-knowledge"),
    ] {
        features.insert(
            name.into(),
            toml::Value::Boolean(!packaged || feature_dir.join(bin).is_file()),
        );
    }
    ctx.insert("features".into(), toml::Value::Table(features));
    Ok(ctx)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_valid_waybar_jsonc(rendered: &str) {
        let body = rendered
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(value["modules-right"].is_array());
    }

    #[test]
    fn packaged_desktop_hides_uninstalled_feature_entries() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let empty = tempfile::tempdir().unwrap();
        let ctx = load_with_feature_dir(root, "black", true, empty.path()).unwrap();
        let env = minijinja::Environment::new();
        for (source, absent) in [
            ("configs/waybar/config.jsonc", "\"custom/sy-power\","),
            ("configs/waybar/config.jsonc", "\"custom/agents\","),
            ("configs/waybar/config.jsonc", "\"custom/sy-syauth\","),
            ("configs/waybar/config.jsonc", "\"custom/sy-knowledge\","),
            ("configs/niri/config.kdl", "\"power\";"),
            ("configs/niri/config.kdl", "\"agt\" \"run\";"),
            ("configs/niri/config.kdl", "\"file\" \"--ipc\""),
        ] {
            let rendered = env
                .render_str(&fs::read_to_string(root.join(source)).unwrap(), &ctx)
                .unwrap();
            if source.ends_with("jsonc") {
                assert_valid_waybar_jsonc(&rendered);
            }
            assert!(!rendered.contains(absent), "{source}: {absent}");
        }
        for bin in [
            "sy-system-power",
            "sy-system-agent",
            "sy-system-auth",
            "sy-filemanager",
            "sy-system-knowledge",
        ] {
            fs::write(empty.path().join(bin), "").unwrap();
        }
        let ctx = load_with_feature_dir(root, "black", true, empty.path()).unwrap();
        let bar = env
            .render_str(
                &fs::read_to_string(root.join("configs/waybar/config.jsonc")).unwrap(),
                &ctx,
            )
            .unwrap();
        assert!(bar.contains("\"custom/sy-power\""));
        assert!(bar.contains("\"custom/sy-syauth\""));
        assert!(bar.contains("\"custom/agents\""));
        assert!(bar.contains("\"custom/sy-knowledge\""));
        assert!(bar.contains("/usr/bin/foot -e /usr/bin/sy gpu"));
        assert!(bar.contains("/usr/bin/foot -e /usr/bin/sy npu"));
        assert_valid_waybar_jsonc(&bar);
    }
}
