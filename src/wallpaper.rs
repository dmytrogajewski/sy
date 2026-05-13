use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context, Result};

/// Set a wallpaper, print the current one, or respawn the daemon from saved
/// state (called from niri's spawn-at-startup).
///
/// With no path and no saved state, the built-in default is applied: the
/// kitten logo centered on a black background. The asset ships in this repo
/// at `configs/sy/assets/logo_w.png` and is materialised under
/// `~/.config/sy/assets/` by `sy apply`. Passing `--default` clears the saved
/// state so the default also persists across reboots.
pub fn run(path: Option<PathBuf>, start: bool, default: bool) -> Result<()> {
    let state = state_path()?;
    let saved = saved_path(&state);

    if default {
        match fs::remove_file(&state) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("rm {}", state.display())),
        }
        apply_default()?;
        let logo = default_image()?;
        println!("(default) {}", logo.display());
        return Ok(());
    }

    if start {
        return match saved {
            Some(p) => apply_user(&p),
            None => apply_default(),
        };
    }

    match path {
        None => match saved {
            Some(p) => {
                println!("{}", p.display());
                Ok(())
            }
            None => {
                let logo = default_image()?;
                apply_default()?;
                println!("(default) {}", logo.display());
                Ok(())
            }
        },
        Some(p) => {
            let abs = p
                .canonicalize()
                .with_context(|| format!("resolve {}", p.display()))?;
            if !abs.is_file() {
                return Err(anyhow!("{} is not a regular file", abs.display()));
            }
            if let Some(parent) = state.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            fs::write(&state, format!("{}\n", abs.display()))
                .with_context(|| format!("write {}", state.display()))?;
            apply_user(&abs)?;
            println!("wallpaper: {}", abs.display());
            Ok(())
        }
    }
}

fn saved_path(state: &Path) -> Option<PathBuf> {
    let s = fs::read_to_string(state).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

fn apply_user(image: &Path) -> Result<()> {
    apply(image, "fill", None)
}

fn apply_default() -> Result<()> {
    let logo = default_image()?;
    if !logo.is_file() {
        return Err(anyhow!(
            "default wallpaper asset {} missing — run `sy apply`",
            logo.display()
        ));
    }
    apply(&logo, "center", Some("#000000"))
}

fn apply(image: &Path, mode: &str, color: Option<&str>) -> Result<()> {
    if !crate::which("swaybg") {
        return Err(anyhow!("swaybg not found on PATH — dnf install swaybg"));
    }
    // Replace any running swaybg so there's a single daemon per session.
    let _ = Command::new("pkill").args(["-x", "swaybg"]).status();
    // Give the compositor a moment to release the layer surfaces so the new
    // daemon paints without a flash of the previous image.
    std::thread::sleep(std::time::Duration::from_millis(80));
    let mut cmd = Command::new("swaybg");
    cmd.args(["-m", mode, "-i"]).arg(image);
    if let Some(c) = color {
        cmd.args(["-c", c]);
    }
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawn swaybg -i {}", image.display()))?;
    Ok(())
}

fn state_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/sy/wallpaper"))
}

fn default_image() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/sy/assets/logo_w.png"))
}
