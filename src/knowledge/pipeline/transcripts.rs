//! Claude-transcripts pipeline: one [`Record`] per transcript turn.
//!
//! Claude Code stores each session as a line-delimited JSON file under
//! `~/.claude/projects/<project>/<session>.jsonl`. Every line is one
//! turn/event object. This pipeline emits one record per parseable line,
//! carrying a structured payload (`date` from the line's timestamp,
//! `from` = role, `model`, `project_id`) so the index can filter on them.
//! The source kind (`claude-transcripts`) is applied downstream from the
//! source registry (`select` routes here, `build_point` stamps the kind),
//! which is what Step 11 default-excludes.
//!
//! Tolerance is required (REQ-4 transcripts): a malformed or blank line is
//! skipped, never fatal — a file with some bad lines still yields records
//! for the good ones.

use super::{Pipeline, Record, RecordPayload};
use crate::knowledge::chunk;

/// Emits one record per line of a Claude Code `.jsonl` transcript.
pub struct TranscriptsPipeline;

impl Pipeline for TranscriptsPipeline {
    fn records(&self, key: &str, text: &str) -> Vec<Record> {
        let project_id = project_id_from_key(key);
        text.lines()
            .enumerate()
            .filter_map(|(i, line)| turn_record(key, i as u32, line, project_id.as_deref()))
            .collect()
    }
}

/// Parse one transcript line into a [`Record`], or `None` when the line is
/// blank, malformed JSON, or carries no usable turn text. Skipping is how
/// the pass stays tolerant of partially-corrupt transcripts.
fn turn_record(key: &str, index: u32, line: &str, project_id: Option<&str>) -> Option<Record> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = turn_text(&v);
    if text.is_empty() {
        return None;
    }
    Some(Record {
        chunk_id: chunk::point_id(key, index),
        payload: RecordPayload {
            chunk_index: index,
            date: string_field(&v, &["timestamp", "ts"]),
            from: string_field(&v, &["role"]),
            model: string_field(&v, &["model"]),
            project_id: project_id.map(str::to_string),
            ..Default::default()
        },
        text,
    })
}

/// Pull the turn's text. Claude lines may store `message.content` as a
/// plain string or as an array of typed blocks (`{type:"text",text:..}`);
/// coalesce either shape into one plain-text body.
fn turn_text(v: &serde_json::Value) -> String {
    let content = v.get("message").and_then(|m| m.get("content"));
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => v
            .get("content")
            .and_then(|c| c.as_str())
            .or_else(|| v.get("text").and_then(|t| t.as_str()))
            .unwrap_or_default()
            .to_string(),
    }
}

/// First present string among `keys`, looked up both at top level and under
/// a nested `message` object (Claude nests `role`/`model` there).
fn string_field(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            return Some(s.to_string());
        }
        if let Some(s) = v
            .get("message")
            .and_then(|m| m.get(k))
            .and_then(|x| x.as_str())
        {
            return Some(s.to_string());
        }
    }
    None
}

/// Extract the `<project>` segment from a `.claude/projects/<project>/..`
/// key. Returns `None` when the key isn't under a projects dir.
fn project_id_from_key(key: &str) -> Option<String> {
    let marker = "projects/";
    let rest = &key[key.find(marker)? + marker.len()..];
    rest.split('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSONL_TWO_TURNS: &str = concat!(
        r#"{"type":"user","timestamp":"2024-01-01T10:00:00Z","message":{"role":"user","content":"hello"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2024-01-02T11:00:00Z","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"text","text":"hi there"}]}}"#,
    );

    const KEY: &str = "~/.claude/projects/-home-me-sources-sy/session.jsonl";

    #[test]
    fn jsonl_emits_one_record_per_turn() {
        let recs = TranscriptsPipeline.records(KEY, JSONL_TWO_TURNS);
        assert_eq!(recs.len(), 2);
        // Turn 1: user text, role+ts populated.
        assert_eq!(recs[0].text, "hello");
        assert_eq!(recs[0].payload.from.as_deref(), Some("user"));
        assert_eq!(
            recs[0].payload.date.as_deref(),
            Some("2024-01-01T10:00:00Z")
        );
        assert_eq!(
            recs[0].payload.project_id.as_deref(),
            Some("-home-me-sources-sy")
        );
        // Turn 2: assistant text coalesced from content blocks; model carried.
        assert_eq!(recs[1].text, "hi there");
        assert_eq!(recs[1].payload.from.as_deref(), Some("assistant"));
        assert_eq!(recs[1].payload.model.as_deref(), Some("claude-opus-4"));
    }

    #[test]
    fn malformed_jsonl_line_is_skipped_not_fatal() {
        // A blank line and a truncated/invalid JSON line sit between two good
        // turns; the good ones must still produce records.
        let mixed = format!(
            "{}\n\n{}\n{}",
            r#"{"timestamp":"2024-01-01T10:00:00Z","message":{"role":"user","content":"first"}}"#,
            r#"{"timestamp":"2024-01-02T"#, // truncated, invalid JSON
            r#"{"timestamp":"2024-01-03T12:00:00Z","message":{"role":"user","content":"third"}}"#,
        );
        let recs = TranscriptsPipeline.records(KEY, &mixed);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].text, "first");
        assert_eq!(recs[1].text, "third");
    }
}
