//! Tolerant streaming parser for a Telegram `result.json` export.
//!
//! Rather than deserialise the whole document (which fails entirely on a
//! truncated multi-GB file), this locates the top-level `"messages"` array
//! and splits it into individual JSON objects by brace depth, parsing each
//! independently. A truncated trailing object is simply dropped, so a file
//! cut off mid-array still yields every complete message before the cut.

use super::Message;
use serde_json::Value;

/// Parse the `messages` array of a Telegram JSON export into [`Message`]s.
/// Returns empty when the body has no `messages` array (caller then tries
/// the HTML fallback).
pub(super) fn parse(text: &str) -> Vec<Message> {
    let Some(start) = array_start(text) else {
        return Vec::new();
    };
    split_objects(&text[start..])
        .iter()
        .filter_map(|obj| serde_json::from_str::<Value>(obj).ok())
        .map(message_from_value)
        .collect()
}

/// Byte offset just past the `[` that opens the `"messages"` array.
fn array_start(text: &str) -> Option<usize> {
    let key = text.find("\"messages\"")?;
    let open = text[key..].find('[')?;
    Some(key + open + 1)
}

/// Split the body of a JSON array (everything after the opening `[`) into
/// the substrings of its top-level objects, honouring string escapes and
/// nested braces. A final unbalanced object is omitted.
fn split_objects(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut obj_start: Option<usize> = None;
    for (i, ch) in body.char_indices() {
        if in_str {
            match ch {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => {
                if depth == 0 {
                    obj_start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = obj_start.take() {
                        out.push(body[s..=i].to_string());
                    }
                }
            }
            ']' if depth == 0 => break,
            _ => {}
        }
    }
    out
}

/// Build a [`Message`] from a parsed message object.
fn message_from_value(v: Value) -> Message {
    Message {
        id: v.get("id").and_then(Value::as_i64),
        date: v.get("date").and_then(Value::as_str).map(str::to_string),
        from: v.get("from").and_then(Value::as_str).map(str::to_string),
        reply_to_id: v.get("reply_to_message_id").and_then(Value::as_i64),
        has_media: has_media(&v),
        voice_media: voice_media(&v),
        text: coalesce_text(v.get("text")),
    }
}

/// Telegram `text` is a string OR an array of `{type,text}` entities (or
/// bare strings). Coalesce entity `text` fields into a plain string.
fn coalesce_text(text: Option<&Value>) -> String {
    match text {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|it| match it {
                Value::String(s) => s.as_str(),
                Value::Object(o) => o.get("text").and_then(Value::as_str).unwrap_or(""),
                _ => "",
            })
            .collect(),
        _ => String::new(),
    }
}

/// Export-relative path of a transcribable voice note or round video, if
/// any. Telegram stores these under `voice_messages/` and
/// `round_video_messages/` and references them via the `voice_message` /
/// `video_message` fields.
fn voice_media(v: &Value) -> Option<String> {
    // Real Telegram JSON exports tag a transcribable message with
    // `"media_type": "voice_message"` | `"video_message"` and carry the
    // export-relative path in `"file"`.
    if let Some(mt) = v.get("media_type").and_then(Value::as_str) {
        if matches!(mt, "voice_message" | "video_message") {
            if let Some(f) = v.get("file").and_then(Value::as_str) {
                return Some(f.to_string());
            }
        }
    }
    // Fallback for exports/fixtures that inline the path directly under the
    // media-type key (`"voice_message": "voice_messages/…"`).
    for key in ["voice_message", "video_message"] {
        if let Some(p) = v.get(key).and_then(Value::as_str) {
            return Some(p.to_string());
        }
    }
    None
}

/// A message carries media when any of the well-known media keys is present.
fn has_media(v: &Value) -> bool {
    const MEDIA_KEYS: [&str; 5] = [
        "photo",
        "voice_message",
        "video_message",
        "file",
        "media_type",
    ];
    MEDIA_KEYS.iter().any(|k| v.get(*k).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_array_text_is_coalesced_into_plain_string() {
        // Telegram emits links/mentions as `text` entity arrays; coalesce
        // their `text` fields (and bare strings) into one string.
        let v: Value =
            serde_json::from_str(r#"["see ", {"type": "link", "text": "here"}, "!"]"#).unwrap();
        assert_eq!(coalesce_text(Some(&v)), "see here!");
    }
}
