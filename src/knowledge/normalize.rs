//! Per-format text normalization. Sits between `extract`'s `fs::read`
//! and the chunker, so HTML markup and JSON envelope keys don't pollute
//! the embeddings.
//!
//! The functions here all return `String` (possibly empty). The caller
//! decides what an empty string means — typically `Skip(Unsupported)`.

use serde_json::Value;

/// Strip HTML tags / scripts / styles, decode entities, keep visible
/// text + alt-text. Width is set to `usize::MAX` so `html2text` doesn't
/// insert hard wraps that confuse the whitespace-token chunker.
pub fn html_to_text(raw: &[u8]) -> String {
    // `html2text::from_read` returns a `Result` in 0.16+; on parse
    // failure (very malformed HTML) we return the empty string so the
    // caller falls through to `Skip(Unsupported)`.
    html2text::from_read(raw, usize::MAX).unwrap_or_default()
}

/// Walk a `serde_json::Value` tree, collecting string leaves into one
/// `\n`-separated buffer. Object keys in `DENY_KEYS` are skipped along
/// with their values. Numbers / bools / nulls are dropped — they're
/// almost always IDs, timestamps, or dimensions that just dilute the
/// embedding.
pub fn json_to_text(v: &Value) -> String {
    let mut out = String::with_capacity(256);
    walk(v, &mut out);
    out
}

/// Run [`json_to_text`] line-by-line over JSONL / NDJSON input. Bad
/// lines are silently skipped (agent histories sometimes contain
/// partial records on shutdown).
pub fn jsonl_to_text(raw: &[u8]) -> String {
    let s = match std::str::from_utf8(raw) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let mut out = String::with_capacity(s.len() / 2);
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let chunk = json_to_text(&v);
        if !chunk.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&chunk);
        }
    }
    out
}

/// Keys that almost always carry envelope / metadata noise rather than
/// user-readable content. Compared case-insensitively. Values under
/// these keys are pruned wholesale (the walker doesn't descend).
const DENY_KEYS: &[&str] = &[
    "id",
    "date",
    "date_unixtime",
    "mtime",
    "ctime",
    "file",
    "thumbnail",
    "photo",
    "sticker",
    "width",
    "height",
    "duration_seconds",
    "mime_type",
    "type",
    "schema_version",
    "version",
    "hash",
    "content_hash",
    "point_id",
];

fn is_denied(key: &str) -> bool {
    DENY_KEYS.iter().any(|d| d.eq_ignore_ascii_case(key))
}

fn walk(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            if !s.is_empty() {
                out.push_str(s);
                out.push('\n');
            }
        }
        Value::Array(arr) => {
            for x in arr {
                walk(x, out);
            }
        }
        Value::Object(map) => {
            for (k, x) in map {
                if is_denied(k) {
                    continue;
                }
                walk(x, out);
            }
        }
        // Numbers, bools, nulls — almost always IDs / timestamps /
        // flags. Drop wholesale.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn html_strips_markup_keeps_text() {
        let raw = "<html><body><div class=\"msg\"><div class=\"text\">Hi <b>там</b>!</div></div><script>x()</script></body></html>".as_bytes();
        let t = html_to_text(raw);
        assert!(t.contains("Hi"));
        assert!(t.contains("там"));
        assert!(!t.contains("script"));
        assert!(!t.contains("class="));
    }

    #[test]
    fn json_walker_emits_strings_skips_metadata() {
        let v = json!({
            "id": 12345,
            "date": "2025-01-01",
            "from": "Alice",
            "text": "hello world",
            "meta": {"width": 1080, "label": "important"},
        });
        let t = json_to_text(&v);
        assert!(t.contains("Alice"));
        assert!(t.contains("hello world"));
        assert!(t.contains("important"));
        assert!(!t.contains("12345"));
        assert!(!t.contains("2025-01-01"));
        assert!(!t.contains("1080"));
    }

    #[test]
    fn json_handles_telegram_text_entities() {
        let v = json!({
            "from": "Alice",
            "text": [
                {"type": "plain", "text": "click "},
                {"type": "link", "text": "https://example.com"},
                {"type": "plain", "text": " now"},
            ],
        });
        let t = json_to_text(&v);
        assert!(t.contains("Alice"));
        assert!(t.contains("click"));
        assert!(t.contains("https://example.com"));
        assert!(t.contains("now"));
    }

    #[test]
    fn jsonl_skips_malformed_lines() {
        let raw = b"{\"text\":\"first\"}\nnot json\n{\"text\":\"third\"}\n";
        let t = jsonl_to_text(raw);
        assert!(t.contains("first"));
        assert!(t.contains("third"));
    }
}
