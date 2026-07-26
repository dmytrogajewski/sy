//! Per-source indexing pipelines.
//!
//! A [`Pipeline`] turns one source file's extracted text into a list of
//! [`Record`]s — the unit the index pass embeds and upserts. Each
//! [`SourceKind`] maps to a pipeline via [`select`]; today every kind
//! routes through the behaviour-preserving [`generic::GenericPipeline`]
//! (which wraps the existing `chunk::chunk` splitter). Per-source
//! pipelines (Telegram per-message, Claude-transcripts per-turn) land in
//! later roadmap steps and only need to be added to [`select`].

pub mod generic;
pub mod telegram;
pub mod transcripts;

use super::sources::SourceKind;

/// Per-chunk metadata a pipeline attaches to each [`Record`]. For the
/// generic pipeline this carries the chunk index that, together with the
/// file key, derives the stable `chunk_id`. Per-source pipelines extend
/// this with their own fields (date/from/message_id/…) in later steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordPayload {
    /// 0-based position of this chunk within its source file. Combined
    /// with the file key it yields the deterministic point id.
    pub chunk_index: u32,
    /// RFC 3339 datetime for the record, when the source carries one
    /// (e.g. a Telegram message `date`). `None` for plain text chunks.
    pub date: Option<String>,
    /// Sender / author display name, when the source carries one.
    pub from: Option<String>,
    /// Source-native message id (e.g. a Telegram message `id`).
    pub message_id: Option<i64>,
    /// Source-native id of the message this one replies to.
    pub reply_to_id: Option<i64>,
    /// Whether the record points at media (photo / voice / video / file).
    pub has_media: Option<bool>,
    /// Model that produced the turn, for Claude-transcript records
    /// (e.g. `claude-opus-4`). `None` for sources without a model.
    pub model: Option<String>,
    /// Project identifier for Claude-transcript records (the
    /// `~/.claude/projects/<project_id>/` segment). `None` otherwise.
    pub project_id: Option<String>,
    /// Per-record kebab `kind` override. `None` means inherit the source's
    /// kind; transcribed Telegram voice notes set `telegram-voice` so they
    /// are filterable independently of their text-message siblings.
    pub kind: Option<String>,
    /// Per-record `file_path` override. `None` means inherit the source
    /// file's key; transcribed voice notes point at the media file itself so
    /// a search hit references the `.ogg`/`.mp4`, not the `result.json`.
    pub file_path: Option<String>,
}

/// One unit of indexable content emitted by a [`Pipeline`]: the text to
/// embed, its per-chunk metadata, and the stable qdrant point id
/// (`chunk_id`). The `chunk_id` is the source of idempotency — re-indexing
/// the same file produces the same ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub text: String,
    pub payload: RecordPayload,
    pub chunk_id: String,
}

/// Turns one source file's extracted `text` into indexable [`Record`]s.
/// `key` is the storage-form file path used to derive stable point ids.
pub trait Pipeline {
    fn records(&self, key: &str, text: &str) -> Vec<Record>;
}

/// Pick the pipeline for a source kind. Every kind currently routes
/// through the generic chunker; per-source pipelines are added here in
/// later steps. An unrecognised/non-special kind falls back to generic.
pub fn select(kind: SourceKind) -> Box<dyn Pipeline> {
    match kind {
        SourceKind::Telegram => Box::new(telegram::TelegramPipeline),
        SourceKind::ClaudeTranscripts => Box::new(transcripts::TranscriptsPipeline),
        _ => Box::new(generic::GenericPipeline),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_returns_generic_for_unknown_kind() {
        // A non-special kind (no dedicated pipeline yet) must still produce
        // records via the generic chunker.
        let pipeline = select(SourceKind::Generic);
        let records = pipeline.records("src/doc.txt", "hello world");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "hello world");
    }
}
