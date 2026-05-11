//! Load active sy theme palette and expose it as iced colors.
//!
//! Mirrors the layout of `themes/<name>.toml`:
//!   [colors]  bg, bg_soft, bg1, bg2, fg, fg_dim, red, orange, yellow, green,
//!             aqua, blue, purple, gray
//!   [ui]      accent
//!
//! Falls back to gruvbox-material if anything is missing.

use std::{env, fs, path::PathBuf};

use anyhow::Result;
use iced::Color;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Palette {
    pub bg: Color,
    pub bg_soft: Color,
    pub bg1: Color,
    pub fg: Color,
    pub fg_dim: Color,
    pub accent: Color,
    pub aqua: Color,
    pub green: Color,
    pub blue: Color,
    pub orange: Color,
    pub red: Color,
}

impl Default for Palette {
    fn default() -> Self {
        // gruvbox-material fallback.
        Self {
            bg: hex("#282828"),
            bg_soft: hex("#32302f"),
            bg1: hex("#3c3836"),
            fg: hex("#ebdbb2"),
            fg_dim: hex("#a89984"),
            accent: hex("#89b482"),
            aqua: hex("#89b482"),
            green: hex("#a9b665"),
            blue: hex("#7daea3"),
            orange: hex("#e78a4e"),
            red: hex("#ea6962"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ThemeFile {
    #[serde(default)]
    colors: Colors,
    #[serde(default)]
    ui: Ui,
}

#[derive(Debug, Default, Deserialize)]
struct Colors {
    bg: Option<String>,
    bg_soft: Option<String>,
    bg1: Option<String>,
    fg: Option<String>,
    fg_dim: Option<String>,
    aqua: Option<String>,
    green: Option<String>,
    blue: Option<String>,
    orange: Option<String>,
    red: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Ui {
    accent: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SyFile {
    theme: Option<String>,
}

pub fn load() -> Result<Palette> {
    let root = match find_root() {
        Ok(r) => r,
        Err(_) => return Ok(Palette::default()),
    };
    let sy_path = root.join("sy.toml");
    let theme_name = if let Ok(s) = fs::read_to_string(&sy_path) {
        toml::from_str::<SyFile>(&s)
            .ok()
            .and_then(|f| f.theme)
            .unwrap_or_else(|| "gruvbox-material".into())
    } else {
        "gruvbox-material".into()
    };
    let theme_path = root.join("themes").join(format!("{theme_name}.toml"));
    let mut p = Palette::default();
    if let Ok(s) = fs::read_to_string(&theme_path) {
        if let Ok(tf) = toml::from_str::<ThemeFile>(&s) {
            apply(&mut p, &tf);
        }
    }
    Ok(p)
}

fn apply(p: &mut Palette, tf: &ThemeFile) {
    if let Some(s) = &tf.colors.bg {
        p.bg = hex(s);
    }
    if let Some(s) = &tf.colors.bg_soft {
        p.bg_soft = hex(s);
    }
    if let Some(s) = &tf.colors.bg1 {
        p.bg1 = hex(s);
    }
    if let Some(s) = &tf.colors.fg {
        p.fg = hex(s);
    }
    if let Some(s) = &tf.colors.fg_dim {
        p.fg_dim = hex(s);
    }
    if let Some(s) = &tf.colors.aqua {
        p.aqua = hex(s);
    }
    if let Some(s) = &tf.colors.green {
        p.green = hex(s);
    }
    if let Some(s) = &tf.colors.blue {
        p.blue = hex(s);
    }
    if let Some(s) = &tf.colors.orange {
        p.orange = hex(s);
    }
    if let Some(s) = &tf.colors.red {
        p.red = hex(s);
    }
    if let Some(s) = &tf.ui.accent {
        p.accent = hex(s);
    }
}

fn hex(s: &str) -> Color {
    let s = s.trim().trim_start_matches('#');
    let parse = |i: usize| -> f32 {
        u8::from_str_radix(&s.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0) as f32 / 255.0
    };
    if s.len() < 6 {
        return Color::BLACK;
    }
    Color {
        r: parse(0),
        g: parse(2),
        b: parse(4),
        a: 1.0,
    }
}

fn find_root() -> Result<PathBuf> {
    if let Ok(r) = env::var("SY_ROOT") {
        if !r.is_empty() {
            return Ok(PathBuf::from(r));
        }
    }
    let mut cur = env::current_dir()?;
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
                if let Ok(home) = env::var("HOME") {
                    let guess = PathBuf::from(home).join("sources/sy");
                    if guess.exists() {
                        return Ok(guess);
                    }
                }
                return Err(anyhow::anyhow!("repo root not found"));
            }
        }
    }
}
