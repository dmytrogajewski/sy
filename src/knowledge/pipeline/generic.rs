//! Generic pipeline: a behaviour-preserving lift of the existing
//! `chunk::chunk` sliding-window splitter into the [`Pipeline`] trait.
//!
//! Output is identical in *scheme* to the pre-pipeline index path: one
//! record per chunk, `chunk_id == chunk::point_id(key, index)` (blake3 of
//! `"<file>::<index>"`). The only deliberate change is the chunk-size
//! target, lowered from 512 to [`GENERIC_CHUNK_TOKENS`] (in the SPEC's
//! 500–800-token band). Smaller chunks shift chunk boundaries and thus the
//! derived point ids for generic content — that re-chunking is expected and
//! is handled by the one-time Step 3 re-index migration, NOT a regression.

use super::{Pipeline, Record, RecordPayload};
use crate::knowledge::chunk;

/// Target chunk size for the generic pipeline, in whitespace tokens.
/// Lowered from the historical 512 into the SPEC's 500–800-token band to
/// tighten retrieval granularity. See module docs for the migration note.
pub const GENERIC_CHUNK_TOKENS: usize = 640;

/// Wraps `chunk::chunk` unchanged; the chunk-size target is the only knob
/// that moved (`GENERIC_CHUNK_TOKENS`).
pub struct GenericPipeline;

impl Pipeline for GenericPipeline {
    fn records(&self, key: &str, text: &str) -> Vec<Record> {
        chunk::chunk_sized(text, GENERIC_CHUNK_TOKENS)
            .into_iter()
            .map(|c| Record {
                chunk_id: chunk::point_id(key, c.index),
                payload: RecordPayload {
                    chunk_index: c.index,
                    ..RecordPayload::default()
                },
                text: c.text,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_emits_records_with_stable_chunk_ids() {
        let key = "src/doc.txt";
        let text = "one two three four five";
        let records = GenericPipeline.records(key, text);
        assert_eq!(records.len(), 1);
        // chunk_id MUST equal blake3("<file>::<index>") so re-indexing a
        // generic source produces the SAME point ids (idempotent).
        assert_eq!(records[0].chunk_id, chunk::point_id(key, 0));
        assert_eq!(records[0].payload.chunk_index, 0);
        assert_eq!(records[0].text, text);
    }
}
