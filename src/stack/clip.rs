//! cliphist adapter — read-only view of the live clipboard history.
//!
//! cliphist's CLI surface:
//!   `cliphist list`             ← lines like "12345\tshort preview..."
//!   `cliphist decode <id>`      ← raw bytes of the entry
//!
//! We expose just the top N entries with a short preview; the bar uses this
//! for tooltip text and to populate the clip section of the bar.

use std::process::{Command, Stdio};

use anyhow::Result;

#[allow(dead_code)] // consumed by the bar (feature-gated) and via the `copy` action
#[derive(Debug, Clone)]
pub struct ClipEntry {
    pub id: String,
    pub preview: String,
    /// Heuristic content kind: "image" if cliphist tags it as binary image,
    /// "text" otherwise.
    pub content_kind: String,
}

/// Read the top `n` entries from cliphist. Returns an empty vec if cliphist
/// is missing or fails (so the bar degrades gracefully).
#[allow(dead_code)]
pub fn top(n: usize) -> Vec<ClipEntry> {
    let out = match Command::new("cliphist")
        .arg("list")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let mut entries = Vec::with_capacity(n);
    for line in s.lines().take(n) {
        // cliphist format: "<id>\t<preview>"
        let mut parts = line.splitn(2, '\t');
        let id = parts.next().unwrap_or("").trim().to_string();
        if id.is_empty() {
            continue;
        }
        let preview = parts.next().unwrap_or("").to_string();
        let content_kind = if preview.starts_with("[[ binary data") {
            "image".to_string()
        } else {
            "text".to_string()
        };
        entries.push(ClipEntry {
            id,
            preview,
            content_kind,
        });
    }
    entries
}

/// Decode a cliphist entry by id, returning raw bytes. Best-effort.
#[allow(dead_code)]
pub fn decode(id: &str) -> Result<Vec<u8>> {
    let out = Command::new("cliphist")
        .arg("decode")
        .arg(id)
        .stderr(Stdio::null())
        .output()?;
    Ok(out.stdout)
}

/// Decode and write to wl-copy (used by the bar's "copy" action).
#[allow(dead_code)]
pub fn copy_to_clipboard(id: &str) -> Result<()> {
    use std::io::Write;
    let bytes = decode(id)?;
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&bytes)?;
    }
    let _ = child.wait();
    Ok(())
}
