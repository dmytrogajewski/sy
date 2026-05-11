use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;

const RATE: u32 = 44100;
const AMP: i16 = 3200;

/// Play a short square-wave blip at `freq_hz` for `ms` milliseconds.
/// Piped to paplay as raw PCM so no temp files are needed.
pub fn blip(freq_hz: f32, ms: u32) -> Result<()> {
    let samples = square(freq_hz, ms);
    let mut child = Command::new("paplay")
        .args(["--raw", "--format=s16le", "--rate=44100", "--channels=1"])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for s in &samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let _ = stdin.write_all(&bytes);
    }
    let _ = child.wait();
    Ok(())
}

fn square(freq: f32, ms: u32) -> Vec<i16> {
    let n = (RATE as f32 * ms as f32 / 1000.0) as usize;
    let mut out = Vec::with_capacity(n);
    let period = RATE as f32 / freq;
    let fade = (n / 8).max(1) as f32;
    let tail = n as f32 - fade;
    for i in 0..n {
        let phase = (i as f32 % period) / period;
        let env = if (i as f32) < fade {
            i as f32 / fade
        } else if (i as f32) > tail {
            (n as f32 - i as f32) / fade
        } else {
            1.0
        };
        let v = if phase < 0.5 { AMP } else { -AMP };
        out.push((v as f32 * env) as i16);
    }
    out
}

// -- session jingles ------------------------------------------------------

/// Play the login jingle (blocking). Silent-gated.
pub fn login() -> Result<()> {
    play_named("login", "SY_SND_LOGIN", false)
}

/// Play the logout jingle (blocking). Silent-gated.
pub fn logout() -> Result<()> {
    play_named("logout", "SY_SND_LOGOUT", false)
}

/// Play login then logout back-to-back, ignoring the silent gate. Used by
/// `sy snd test` to verify both assets without faking the time window.
pub fn test() -> Result<()> {
    play_named("login", "SY_SND_LOGIN", true)?;
    play_named("logout", "SY_SND_LOGOUT", true)
}

fn play_named(name: &str, env_var: &str, force: bool) -> Result<()> {
    if !force {
        if env::var("SY_SND").ok().as_deref() == Some("0") {
            return Ok(());
        }
        if crate::silent::is_active() {
            return Ok(());
        }
    }
    let Some(path) = resolve(env_var, &format!("{name}.wav")) else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }
    play_wav(&path)
}

fn resolve(env_var: &str, default_name: &str) -> Option<PathBuf> {
    if let Ok(p) = env::var(env_var) {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config/sy/assets/sounds")
            .join(default_name),
    )
}

fn play_wav(path: &Path) -> Result<()> {
    let _ = Command::new("paplay")
        .arg(path)
        .stderr(Stdio::null())
        .status();
    Ok(())
}
