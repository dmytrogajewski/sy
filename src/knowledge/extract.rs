//! Text extraction. Per-extension dispatch:
//!
//! - `.pdf` → `pdftotext` shell-out (existing).
//! - `.html` / `.htm` → `html2text` (drops markup, scripts, styles;
//!   keeps visible text + alt-text).
//! - `.json` / `.jsonl` / `.ndjson` → walk the value tree, collect
//!   string leaves, skip metadata keys (id / date / dimensions).
//! - Everything else → raw UTF-8 read.
//!
//! The post-extract bytes feed `chunk` and `embed`, and `state::hash_bytes`
//! takes its content hash from this output. So changing the normalizer
//! for a given extension automatically triggers a re-embed of that
//! file's chunks on the next index pass.

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use super::normalize;

pub const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug)]
pub enum Extracted {
    /// Plain UTF-8 text (source code, markdown, plain text, ...).
    Text(String),
    /// Skipped — file isn't indexable in v1 (binary, oversize, unknown).
    Skip(SkipReason),
}

#[derive(Debug)]
pub enum SkipReason {
    Binary,
    TooLarge,
    Unsupported,
    PdfToTextMissing,
    PdfFailed(String),
    ReadFailed(String),
}

impl SkipReason {
    pub fn label(&self) -> &'static str {
        match self {
            SkipReason::Binary => "binary",
            SkipReason::TooLarge => "too-large",
            SkipReason::Unsupported => "unsupported-ext",
            SkipReason::PdfToTextMissing => "pdftotext-missing",
            SkipReason::PdfFailed(_) => "pdf-failed",
            SkipReason::ReadFailed(_) => "read-failed",
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            SkipReason::PdfFailed(s) | SkipReason::ReadFailed(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

pub fn extract_with_limit(path: &Path, max_bytes: u64) -> Result<Extracted> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.len() > max_bytes {
        return Ok(Extracted::Skip(SkipReason::TooLarge));
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "pdf" => extract_pdf(path),
        "html" | "htm" => extract_html(path),
        "json" => extract_json(path),
        "jsonl" | "ndjson" => extract_jsonl(path),
        _ => extract_text(path),
    }
}

fn extract_html(path: &Path) -> Result<Extracted> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => return Ok(Extracted::Skip(SkipReason::ReadFailed(e.to_string()))),
    };
    let text = normalize::html_to_text(&bytes);
    if text.trim().is_empty() {
        Ok(Extracted::Skip(SkipReason::Unsupported))
    } else {
        Ok(Extracted::Text(text))
    }
}

fn extract_json(path: &Path) -> Result<Extracted> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => return Ok(Extracted::Skip(SkipReason::ReadFailed(e.to_string()))),
    };
    // Falls back to the raw read on parse failure — at worst we get the
    // pre-change behaviour, never worse.
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return extract_text(path),
    };
    let text = normalize::json_to_text(&value);
    if text.trim().is_empty() {
        Ok(Extracted::Skip(SkipReason::Unsupported))
    } else {
        Ok(Extracted::Text(text))
    }
}

fn extract_jsonl(path: &Path) -> Result<Extracted> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => return Ok(Extracted::Skip(SkipReason::ReadFailed(e.to_string()))),
    };
    let text = normalize::jsonl_to_text(&bytes);
    if text.trim().is_empty() {
        // Empty JSONL after stripping — fall back to raw text so we
        // don't drop the file silently.
        return extract_text(path);
    }
    Ok(Extracted::Text(text))
}

fn extract_text(path: &Path) -> Result<Extracted> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => return Ok(Extracted::Skip(SkipReason::ReadFailed(e.to_string()))),
    };
    match String::from_utf8(bytes) {
        Ok(s) => {
            if s.chars().take(1024).filter(|c| *c == '\0').count() > 0 {
                Ok(Extracted::Skip(SkipReason::Binary))
            } else if s.trim().is_empty() {
                Ok(Extracted::Skip(SkipReason::Unsupported))
            } else {
                Ok(Extracted::Text(s))
            }
        }
        Err(_) => Ok(Extracted::Skip(SkipReason::Binary)),
    }
}

fn extract_pdf(path: &Path) -> Result<Extracted> {
    if !crate::which("pdftotext") {
        return Ok(Extracted::Skip(SkipReason::PdfToTextMissing));
    }
    let out = match Command::new("pdftotext")
        .args(["-q", "-layout"])
        .arg(path)
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(e) => return Ok(Extracted::Skip(SkipReason::PdfFailed(e.to_string()))),
    };
    if !out.status.success() {
        return Ok(Extracted::Skip(SkipReason::PdfFailed(format!(
            "exit {:?}",
            out.status.code()
        ))));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.trim().is_empty() {
        Ok(Extracted::Skip(SkipReason::Unsupported))
    } else {
        Ok(Extracted::Text(text))
    }
}
