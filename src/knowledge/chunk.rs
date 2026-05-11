//! Sliding-window chunker. We measure chunk size in whitespace-tokens
//! (close enough to model tokens for routing decisions; embedding model
//! handles its own tokenisation internally up to 512 tokens).

use serde::Serialize;

const CHUNK_TOKENS: usize = 512;
const OVERLAP_TOKENS: usize = 64;

#[derive(Debug, Clone, Serialize)]
pub struct Chunk {
    pub index: u32,
    pub text: String,
}

pub fn chunk(text: &str) -> Vec<Chunk> {
    // Tokenise on whitespace, keeping byte ranges so we can splice without
    // per-chunk allocation explosions.
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() {
        return Vec::new();
    }

    let step = CHUNK_TOKENS.saturating_sub(OVERLAP_TOKENS).max(1);
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut idx = 0u32;
    while start < tokens.len() {
        let end = (start + CHUNK_TOKENS).min(tokens.len());
        let body = tokens[start..end].join(" ");
        out.push(Chunk {
            index: idx,
            text: body,
        });
        idx += 1;
        if end == tokens.len() {
            break;
        }
        start += step;
    }
    out
}

/// Stable point id for a chunk (blake3 hex of "<file_path>::<chunk_index>").
/// Qdrant accepts hex string ids.
pub fn point_id(file_path: &str, chunk_index: u32) -> String {
    let key = format!("{file_path}::{chunk_index}");
    let h = blake3::hash(key.as_bytes());
    // Qdrant's "uuid-ish" point id format accepts a hex string; we use the
    // first 32 hex chars (128 bits) formatted as a UUID for clarity.
    let hex = h.to_hex();
    let s = &hex[..32];
    format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )
}
