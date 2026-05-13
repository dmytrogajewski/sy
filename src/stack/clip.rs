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

#[derive(Debug, Clone)]
pub struct ClipEntry {
    pub id: String,
    pub preview: String,
    /// Heuristic content kind: "image" if cliphist tags it as binary image,
    /// "text" otherwise.
    pub content_kind: String,
    /// For binary image entries, the extension cliphist reported
    /// inside the preview token (`png`, `jpeg`, …). `None` for text
    /// entries or unrecognised binary types — the bar falls back to
    /// the generic file-media glyph in that case.
    pub image_ext: Option<&'static str>,
}

/// Read the top `n` entries from cliphist. Returns an empty vec if cliphist
/// is missing or fails (so the bar degrades gracefully).
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
        let image_ext = parse_image_ext(&preview);
        let content_kind = if image_ext.is_some() || preview.starts_with("[[ binary data") {
            "image".to_string()
        } else {
            "text".to_string()
        };
        entries.push(ClipEntry {
            id,
            preview,
            content_kind,
            image_ext,
        });
    }
    entries
}

/// Decode a cliphist entry by id, returning raw bytes. Best-effort.
pub fn decode(id: &str) -> Result<Vec<u8>> {
    let out = Command::new("cliphist")
        .arg("decode")
        .arg(id)
        .stderr(Stdio::null())
        .output()?;
    Ok(out.stdout)
}

/// Decode and write to wl-copy (used by the bar's "copy" action).
pub fn copy_to_clipboard(id: &str) -> Result<()> {
    use std::io::Write;
    let bytes = decode(id)?;
    let mut child = Command::new("wl-copy").stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&bytes)?;
    }
    let _ = child.wait();
    Ok(())
}

/// Image extensions cliphist reports in its binary-data preview line.
/// Order matches descending popularity; the parser stops on first
/// match. Bare strings (no leading dot) — cliphist writes them inline
/// in the `[[ binary data N KiB <ext> ]]` token.
const CLIPHIST_IMAGE_EXTS: &[&str] = &["png", "jpeg", "jpg", "webp", "gif", "bmp"];

/// Extract the image extension from a cliphist preview line.
///
/// cliphist 0.7+ formats binary clipboard entries as
/// `[[ binary data N KiB <ext> ]]`. Returns the matched extension as
/// a static string, or `None` for text/unknown entries. Matches the
/// `cliphist-fuzzel-img` contrib script's regex but without pulling a
/// `regex` dep: we only need a fixed extension set.
pub fn parse_image_ext(preview: &str) -> Option<&'static str> {
    let preview = preview.trim_start();
    if !preview.starts_with("[[ binary data") {
        return None;
    }
    let lower = preview.to_ascii_lowercase();
    CLIPHIST_IMAGE_EXTS
        .iter()
        .copied()
        .find(|ext| lower.contains(ext))
}

/// Cache a `size×size` thumbnail of a cliphist image payload at
/// `cache_dir/clip-<id>.<size>.png`. The `clip-` prefix keeps clip
/// thumbnails from colliding with stack item ids (which are 8-hex).
/// Memory-decodes the bytes via `image::load_from_memory`, so any
/// format the `image` crate understands is fair game. Split from
/// `decode_to_thumb_at` so tests can exercise the cache invariant
/// without a fake `cliphist` on `PATH`.
pub fn thumb_from_clip_bytes_at(
    cache_dir: &std::path::Path,
    id: &str,
    bytes: &[u8],
    size: u32,
) -> Result<std::path::PathBuf> {
    let src_img = image::load_from_memory(bytes)
        .map_err(|e| anyhow::anyhow!("decode clip {id} as image: {e}"))?;
    let key = format!("clip-{id}");
    super::state::write_thumbnail_png(cache_dir, &key, src_img, size)
}

/// Shell out to `cliphist decode <id>` for the raw bytes, then run
/// them through `thumb_from_clip_bytes_at`. `ext` is accepted for
/// symmetry with `parse_image_ext` callers but not consulted — the
/// cache always re-encodes as PNG.
pub fn decode_to_thumb_at(
    cache_dir: &std::path::Path,
    id: &str,
    _ext: &str,
    size: u32,
) -> Result<std::path::PathBuf> {
    // Skip the cliphist shell-out entirely when the thumbnail is
    // already cached. The bar repaints every 1 s and would otherwise
    // call `cliphist decode <id>` 8× per second for the default 8
    // clip slots.
    let dest = cache_dir.join(format!("clip-{id}.{size}.png"));
    if dest.exists() {
        return Ok(dest);
    }
    let bytes = decode(id)?;
    thumb_from_clip_bytes_at(cache_dir, id, &bytes, size)
}

/// Convenience wrapper that targets the shared
/// `state::thumbs_dir()` cache.
pub fn decode_to_thumb(id: &str, ext: &str, size: u32) -> Result<std::path::PathBuf> {
    let dir = super::state::thumbs_dir()?;
    decode_to_thumb_at(&dir, id, ext, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_image_ext_handles_png_jpeg_webp_gif_bmp() {
        let cases = [
            ("[[ binary data 100 KiB png ]]", Some("png")),
            ("[[ binary data 12 KiB jpeg ]]", Some("jpeg")),
            ("[[ binary data 9 KiB jpg ]]", Some("jpg")),
            ("[[ binary data 80 KiB webp ]]", Some("webp")),
            ("[[ binary data 3 MiB gif ]]", Some("gif")),
            ("[[ binary data 1 KiB bmp ]]", Some("bmp")),
        ];
        for (input, want) in cases {
            assert_eq!(parse_image_ext(input), want, "input: {input:?}");
        }
    }

    #[test]
    fn parse_image_ext_returns_none_for_text() {
        assert_eq!(parse_image_ext("hello world"), None);
        assert_eq!(parse_image_ext(""), None);
        assert_eq!(parse_image_ext("[[ binary data 5 KiB pdf ]]"), None);
    }

    #[test]
    fn decode_to_thumb_caches_under_clip_prefix() {
        // Build an 8×8 RGB PNG in memory, feed it through the
        // bytes-taking helper, and assert the cache lands at
        // `clip-<id>.<size>.png`. Bypasses `cliphist decode` so we
        // don't need a shim on $PATH.
        let tmp = tempfile::tempdir().unwrap();
        let img: image::RgbImage =
            image::ImageBuffer::from_fn(8, 8, |_, _| image::Rgb([10, 20, 30]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode test png");
        let dest =
            thumb_from_clip_bytes_at(tmp.path(), "55", &bytes, 20).expect("thumbnail success");
        assert_eq!(
            dest.file_name().and_then(|s| s.to_str()),
            Some("clip-55.20.png")
        );
        assert!(dest.exists());
    }
}
