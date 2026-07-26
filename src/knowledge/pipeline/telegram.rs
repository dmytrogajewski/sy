//! Telegram pipeline: consecutive messages coalesced into conversation
//! windows, one [`Record`] per window.
//!
//! Telegram Desktop exports a chat as `result.json` (primary) or a set of
//! `messages*.html` files (fallback).
//!
//! **Why windows, not one-record-per-message.** Telegram chat is mostly
//! tiny messages — a single word, an emoji, "ок", "День". Embedding each as
//! its own point floods the index with contextless single-token vectors that
//! match nothing usefully and crowd out the informative passages (a real
//! corpus showed top hits like the bare word `"День"`). We therefore pack
//! consecutive messages into ~[`WINDOW_TOKENS`]-sized windows, each line
//! prefixed with its sender, so every embedded chunk carries real
//! conversational context and is readable back as a coherent excerpt.
//!
//! Window metadata (`date`, `from`, `message_id`, `reply_to_id`) is anchored
//! on the window's FIRST message; `has_media` is OR-ed across the window.
//! Windows are time-contiguous, so anchoring keeps `date_from`/`date_to` and
//! `from` filters meaningful at window granularity. The `chunk_id` is keyed
//! on that first message id, so a re-index of the same export is idempotent
//! and appending new messages only rewrites the trailing window.
//!
//! Tolerance is the core requirement (REQ-4 telegram): real exports can be
//! multi-GB and this user's `result.json` is invalid past ~25 MB. The JSON
//! path therefore parses the `messages` array **object-by-object** by
//! brace-depth rather than deserialising the whole document — a truncation
//! partway through yields the messages parsed so far instead of panicking
//! or returning zero. When the body is not parseable as a Telegram JSON
//! export at all (or is an HTML export), it falls back to the HTML parser.

use std::path::Path;

use super::{Pipeline, Record, RecordPayload};
use crate::knowledge::{chunk, transcribe::Transcriber, transcribe::transcribe_cached};

mod html;
mod json;

/// Kebab `kind` stamped onto transcribed voice/video chunks so they are
/// filterable independently of their text-message siblings (REQ-5).
pub const VOICE_KIND: &str = "telegram-voice";

/// Target window size in whitespace tokens (the unit
/// [`chunk::chunk_sized`](crate::knowledge::chunk) uses). ~150 tokens is a
/// conversational paragraph that stays well inside the embedder's 512-token
/// budget for Cyrillic subword tokenisation while giving each chunk real
/// context. A window flushes once adding the next message would exceed this.
const WINDOW_TOKENS: usize = 150;

/// Hard cap on messages per window so a burst of one-word messages can't pack
/// one pathologically long chunk that dilutes the embedding.
const WINDOW_MAX_MSGS: usize = 60;

/// Coalesce parsed messages into conversation windows (see module docs).
/// Messages with empty text contribute their `has_media` flag to the window
/// but no text line; a window whose messages are all empty yields nothing.
fn window_records(msgs: Vec<Message>, key: &str) -> Vec<Record> {
    let mut out = Vec::new();
    let mut cur: Vec<Message> = Vec::new();
    let mut cur_tokens = 0usize;
    let mut idx = 0u32;
    for m in msgs {
        let tokens = m.text.split_whitespace().count();
        if !cur.is_empty() && (cur_tokens + tokens > WINDOW_TOKENS || cur.len() >= WINDOW_MAX_MSGS) {
            if let Some(rec) = window_into_record(&cur, key, idx) {
                out.push(rec);
                idx += 1;
            }
            cur.clear();
            cur_tokens = 0;
        }
        cur_tokens += tokens;
        cur.push(m);
    }
    if let Some(rec) = window_into_record(&cur, key, idx) {
        out.push(rec);
    }
    out
}

/// Build one [`Record`] from a window of messages, anchoring metadata on the
/// first message. Returns `None` when the window carries no text at all.
fn window_into_record(msgs: &[Message], key: &str, idx: u32) -> Option<Record> {
    let anchor = msgs.first()?;
    let mut lines = Vec::new();
    for m in msgs {
        let t = m.text.trim();
        if t.is_empty() {
            continue;
        }
        match m.from.as_deref() {
            Some(f) => lines.push(format!("{f}: {t}")),
            None => lines.push(t.to_string()),
        }
    }
    if lines.is_empty() {
        return None;
    }
    let id_for_chunk = anchor.id.map(|i| i as u32).unwrap_or(idx);
    Some(Record {
        chunk_id: chunk::point_id(key, id_for_chunk),
        payload: RecordPayload {
            chunk_index: idx,
            date: anchor.date.clone(),
            from: anchor.from.clone(),
            message_id: anchor.id,
            reply_to_id: anchor.reply_to_id,
            has_media: Some(msgs.iter().any(|m| m.has_media)),
            ..Default::default()
        },
        text: lines.join("\n"),
    })
}

/// Routes a source file's text to the JSON parser when it looks like a
/// Telegram `result.json` (`messages` array), else the HTML parser.
pub struct TelegramPipeline;

impl Pipeline for TelegramPipeline {
    fn records(&self, key: &str, text: &str) -> Vec<Record> {
        let msgs = json::parse(text);
        let msgs = if msgs.is_empty() {
            html::parse(text)
        } else {
            msgs
        };
        window_records(msgs, key)
    }
}

impl TelegramPipeline {
    /// Transcribe every voice note / round video referenced by the export
    /// and emit one `kind=telegram-voice` [`Record`] per media file. Media
    /// paths are resolved relative to the export root (the directory holding
    /// `result.json`, derived from `key`). Transcription is cached
    /// content-addressed next to the media, so already-transcribed files
    /// short-circuit (`transcribe_cached`) and a re-index skips them. Media
    /// that is missing on disk or fails to transcribe is dropped, never
    /// fatal, so one bad file never aborts the pass.
    pub fn voice_records(
        &self,
        key: &str,
        text: &str,
        transcriber: &dyn Transcriber,
    ) -> Vec<Record> {
        let base = Path::new(key)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let msgs = json::parse(text);
        let msgs = if msgs.is_empty() {
            html::parse(text)
        } else {
            msgs
        };
        msgs.into_iter()
            .filter_map(|m| {
                let rel = m.voice_media.as_deref()?;
                let media = base.join(rel);
                let transcript = transcribe_cached(transcriber, &media).ok()?;
                Some(m.into_voice_record(&media.display().to_string(), transcript))
            })
            .collect()
    }
}

/// A single parsed Telegram message, source-format agnostic. Both the JSON
/// and HTML parsers produce these; [`window_into_record`] derives the
/// stable `chunk_id` and payload.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Message {
    pub id: Option<i64>,
    pub date: Option<String>,
    pub from: Option<String>,
    pub reply_to_id: Option<i64>,
    pub has_media: bool,
    /// Export-relative path of a voice note / round video, when the message
    /// carries one (`voice_message` / `video_message`). Drives Step-16
    /// transcription into `kind=telegram-voice` chunks.
    pub voice_media: Option<String>,
    pub text: String,
}

impl Message {
    /// Derive a `kind=telegram-voice` [`Record`] from this message's media.
    /// `media_key` is the resolved media file path; the transcript becomes
    /// the embedded text and the payload carries the message metadata plus
    /// `has_media=true`. The `chunk_id` is keyed on the media path so it is
    /// stable and distinct from the message's own text chunk.
    fn into_voice_record(self, media_key: &str, transcript: String) -> Record {
        Record {
            chunk_id: chunk::point_id(media_key, 0),
            payload: RecordPayload {
                date: self.date,
                from: self.from,
                message_id: self.id,
                reply_to_id: self.reply_to_id,
                has_media: Some(true),
                kind: Some(VOICE_KIND.to_string()),
                file_path: Some(media_key.to_string()),
                ..Default::default()
            },
            text: transcript,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON_TWO_MESSAGES: &str = r#"{
        "name": "Chat",
        "type": "personal_chat",
        "messages": [
            {"id": 11, "type": "message", "date": "2024-01-01T10:00:00",
             "date_unixtime": "1704103200", "from": "Alice", "text": "hello"},
            {"id": 12, "type": "message", "date": "2024-01-02T11:00:00",
             "date_unixtime": "1704189600", "from": "Bob",
             "text": "world", "reply_to_message_id": 11, "photo": "photos/1.jpg"}
        ]
    }"#;

    #[test]
    fn json_export_coalesces_short_messages_into_one_window() {
        // Two short messages pack into a single conversation window with
        // sender-prefixed lines — not two contextless single-word points.
        let recs = TelegramPipeline.records("tg/result.json", JSON_TWO_MESSAGES);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text, "Alice: hello\nBob: world");
        // Metadata anchors on the FIRST message of the window.
        assert_eq!(recs[0].payload.message_id, Some(11));
        assert_eq!(recs[0].payload.from.as_deref(), Some("Alice"));
        assert_eq!(recs[0].payload.date.as_deref(), Some("2024-01-01T10:00:00"));
    }

    #[test]
    fn window_ors_has_media_across_its_messages() {
        // The window contains a photo-bearing message (msg 12), so the
        // window is flagged `has_media` even though its anchor (msg 11)
        // carries none.
        let recs = TelegramPipeline.records("tg/result.json", JSON_TWO_MESSAGES);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].payload.has_media, Some(true));
        // Anchor metadata: msg 11 has no reply link.
        assert_eq!(recs[0].payload.reply_to_id, None);
    }

    #[test]
    fn long_messages_split_into_multiple_windows() {
        // Each message is > WINDOW_TOKENS, so every message becomes its own
        // window (the next message always overflows the running window).
        let big = "word ".repeat(WINDOW_TOKENS + 5);
        let body = format!(
            r#"{{"messages":[
                {{"id":1,"type":"message","from":"A","text":"{big}"}},
                {{"id":2,"type":"message","from":"B","text":"{big}"}}
            ]}}"#
        );
        let recs = TelegramPipeline.records("tg/result.json", &body);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].payload.message_id, Some(1));
        assert_eq!(recs[1].payload.message_id, Some(2));
    }

    #[test]
    fn empty_text_messages_do_not_emit_empty_chunks() {
        // A media-only message (empty text) must not create an empty-text
        // point; it only contributes `has_media` to a window with real text.
        let body = r#"{"messages":[
            {"id":1,"type":"message","from":"A","text":"","photo":"p/1.jpg"},
            {"id":2,"type":"message","from":"A","text":"actual content here"}
        ]}"#;
        let recs = TelegramPipeline.records("tg/result.json", body);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text, "A: actual content here");
        assert_eq!(recs[0].payload.has_media, Some(true));
    }

    #[test]
    fn all_empty_messages_yield_no_records() {
        let body = r#"{"messages":[
            {"id":1,"type":"message","from":"A","text":""},
            {"id":2,"type":"message","from":"B","text":"   "}
        ]}"#;
        let recs = TelegramPipeline.records("tg/result.json", body);
        assert!(recs.is_empty());
    }

    #[test]
    fn truncated_result_json_yields_partial_records_without_panicking() {
        // Cut the fixture mid-way through the SECOND message object.
        let cut = JSON_TWO_MESSAGES.find("\"reply_to_message_id\"").unwrap();
        let truncated = &JSON_TWO_MESSAGES[..cut];
        let recs = TelegramPipeline.records("tg/result.json", truncated);
        // The first (complete) message survives; the partial one is dropped.
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].payload.message_id, Some(11));
    }

    const HTML_EXPORT: &str = r#"
    <div class="message default clearfix" id="message5">
      <div class="from_name">Carol</div>
      <div class="date details" title="01.01.2024 12:00:00 UTC+00:00">12:00</div>
      <div class="text">hi from html</div>
      <a class="media_photo" href="photos/2.jpg"></a>
    </div>
    <div class="message default clearfix" id="message6">
      <div class="from_name">Dave</div>
      <div class="date details" title="02.01.2024 13:00:00 UTC+00:00">13:00</div>
      <div class="text">no media here</div>
    </div>"#;

    struct FakeTranscriber(&'static str);
    impl crate::knowledge::transcribe::Transcriber for FakeTranscriber {
        fn transcribe(&self, _media: &std::path::Path) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }

    const VOICE_FIXTURE: &str = r#"{
        "messages": [
            {"id": 7, "type": "message", "date": "2024-01-03T09:00:00",
             "from": "Eve", "voice_message": "voice_messages/file_7.ogg",
             "media_type": "voice_message", "text": ""}
        ]
    }"#;

    #[test]
    fn voice_media_emits_transcript_chunk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let media_dir = dir.path().join("voice_messages");
        std::fs::create_dir_all(&media_dir).expect("mkdir");
        std::fs::write(media_dir.join("file_7.ogg"), b"audio").expect("media");
        let key = dir.path().join("result.json").display().to_string();

        let recs = TelegramPipeline.voice_records(&key, VOICE_FIXTURE, &FakeTranscriber("привет"));
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text, "привет");
        // Tagged so it is filterable independently of text messages.
        assert_eq!(recs[0].payload.kind.as_deref(), Some("telegram-voice"));
        // Payload points back at the source media file.
        assert!(recs[0].payload.from.as_deref() == Some("Eve"));
    }

    /// Real Telegram JSON exports do NOT inline the path under a
    /// `"voice_message"` key; they tag the message `"media_type":
    /// "voice_message"` (or `"video_message"` for round videos) and carry
    /// the export-relative path in `"file"`. The parser must read that
    /// shape, otherwise every real voice note is silently skipped.
    const REAL_EXPORT_FIXTURE: &str = r#"{
        "messages": [
            {"id": 44050, "type": "message", "date": "2020-10-19T20:15:34",
             "from": "Лу", "file": "voice_messages/audio_1.ogg",
             "media_type": "voice_message", "mime_type": "audio/ogg",
             "duration_seconds": 11, "text": ""},
            {"id": 44051, "type": "message", "date": "2020-10-19T20:16:00",
             "from": "Лу", "file": "round_video_messages/video_1.mp4",
             "media_type": "video_message", "mime_type": "video/mp4", "text": ""}
        ]
    }"#;

    #[test]
    fn voice_media_reads_media_type_and_file_from_real_export() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (sub, name) in [
            ("voice_messages", "audio_1.ogg"),
            ("round_video_messages", "video_1.mp4"),
        ] {
            let d = dir.path().join(sub);
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(d.join(name), b"audio").expect("media");
        }
        let key = dir.path().join("result.json").display().to_string();

        let recs =
            TelegramPipeline.voice_records(&key, REAL_EXPORT_FIXTURE, &FakeTranscriber("привет"));
        assert_eq!(recs.len(), 2, "both voice + round-video must transcribe");
        assert!(recs.iter().all(|r| r.text == "привет"));
        assert!(recs
            .iter()
            .all(|r| r.payload.kind.as_deref() == Some("telegram-voice")));
    }

    #[test]
    fn already_transcribed_media_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let media_dir = dir.path().join("voice_messages");
        std::fs::create_dir_all(&media_dir).expect("mkdir");
        let media = media_dir.join("file_7.ogg");
        std::fs::write(&media, b"audio").expect("media");
        let key = dir.path().join("result.json").display().to_string();

        // Pre-seed the content-addressed sidecar; the transcriber must not run.
        let hash = blake3::hash(b"audio").to_hex().to_string();
        let sidecar = crate::knowledge::transcribe::sidecar_path(&media, &hash);
        std::fs::write(&sidecar, "cached-text").expect("sidecar");

        struct Panicking;
        impl crate::knowledge::transcribe::Transcriber for Panicking {
            fn transcribe(&self, _m: &std::path::Path) -> anyhow::Result<String> {
                panic!("must not transcribe already-cached media")
            }
        }
        let recs = TelegramPipeline.voice_records(&key, VOICE_FIXTURE, &Panicking);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text, "cached-text");
    }

    #[test]
    fn falls_back_to_html_when_json_invalid() {
        let recs = TelegramPipeline.records("tg/messages.html", HTML_EXPORT);
        // Both short HTML messages coalesce into one window; the HTML parser
        // still fed the metadata (anchored on the first message) and the
        // window OR-es in the photo from message 5.
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text, "Carol: hi from html\nDave: no media here");
        assert_eq!(recs[0].payload.from.as_deref(), Some("Carol"));
        assert_eq!(
            recs[0].payload.date.as_deref(),
            Some("01.01.2024 12:00:00 UTC+00:00")
        );
        assert_eq!(recs[0].payload.has_media, Some(true));
    }
}
