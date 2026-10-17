//! Sliding-window chunker. Chunk size is budgeted two ways at once, because
//! a whitespace-token budget alone is not a bound:
//!
//! 1. a token target, chosen per pipeline (`chunk_sized`'s `chunk_tokens`);
//! 2. a hard **character** ceiling, [`MAX_CHUNK_CHARS`].
//!
//! The ceiling is what makes the chunker honest about what the embedder can
//! actually see. `multilingual-e5-base` is exported at a static `(1, 512)`
//! shape and `aiplane::workloads::embed::encode` does `ids.truncate(512)`, so
//! everything past 512 *wordpieces* is silently discarded — it is indexed,
//! never embedded, and therefore unreachable by search while looking
//! perfectly healthy. XLM-R's sentencepiece averages ~3.5 chars per wordpiece
//! across Cyrillic and Latin prose, so 512 wordpieces is ~1.8 k chars, which
//! is the cap below.
//!
//! Without it, whitespace-poor payloads break the token budget entirely: a
//! minified JSON or base64 blob is a *single* whitespace token, so this
//! corpus grew one chunk of 1 039 923 characters (99.95 % of which no query
//! could ever match). Splitting on characters instead keeps every chunk
//! inside the window and turns the remainder into searchable chunks.

use serde::Serialize;

const OVERLAP_TOKENS: usize = 64;

/// Hard ceiling on one chunk, in `char`s (~512 XLM-R wordpieces — see the
/// module docs). Bounds are in `char`s rather than bytes so non-ASCII corpora
/// get the same guarantee as ASCII ones.
pub const MAX_CHUNK_CHARS: usize = 1_800;

#[derive(Debug, Clone, Serialize)]
pub struct Chunk {
    pub index: u32,
    pub text: String,
}

/// Sliding-window chunker with a caller-chosen target chunk size (in
/// whitespace tokens). The generic pipeline passes its own
/// `GENERIC_CHUNK_TOKENS` target. Overlap is fixed at [`OVERLAP_TOKENS`];
/// the id scheme (see [`point_id`]) is unchanged.
pub fn chunk_sized(text: &str, chunk_tokens: usize) -> Vec<Chunk> {
    let chunk_tokens = chunk_tokens.max(1);
    // Whitespace-split, then cut any oversized token so the char ceiling is
    // absolute rather than advisory.
    let mut pieces: Vec<&str> = Vec::new();
    for token in text.split_whitespace() {
        pieces.extend_from_slice(&split_oversized(token));
    }
    if pieces.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut start = 0usize;
    let mut idx = 0u32;
    while start < pieces.len() {
        // Grow the window under both budgets. The token target binds for
        // normal prose; the char cap binds for dense payloads.
        let mut end = start;
        let mut chars = 0usize;
        while end < pieces.len() && (end - start) < chunk_tokens {
            // +1 char per joiner in `pieces[..].join(" ")`.
            let add = pieces[end].chars().count() + usize::from(end > start);
            if end > start && chars + add > MAX_CHUNK_CHARS {
                break;
            }
            chars += add;
            end += 1;
        }
        out.push(Chunk {
            index: idx,
            text: pieces[start..end].join(" "),
        });
        idx += 1;
        if end == pieces.len() {
            break;
        }
        // Overlap is measured in pieces, so it survives a window that the
        // char cap cut short; `max(1)` keeps a single oversized piece from
        // stalling the walk.
        start += (end - start).saturating_sub(OVERLAP_TOKENS).max(1);
    }
    out
}

/// Cut one whitespace token into [`MAX_CHUNK_CHARS`]-sized pieces on char
/// boundaries. Returns a one-element slice for every token that already fits,
/// so ordinary text is untouched and keeps its identity.
fn split_oversized(token: &str) -> Vec<&str> {
    if token.chars().count() <= MAX_CHUNK_CHARS {
        return vec![token];
    }
    let mut pieces = Vec::new();
    let mut rest = token;
    while rest.chars().count() > MAX_CHUNK_CHARS {
        let cut = rest
            .char_indices()
            .nth(MAX_CHUNK_CHARS)
            .map(|(byte, _)| byte)
            .unwrap_or(rest.len());
        pieces.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this ceiling exists for: a whitespace-poor payload (minified
    /// JSON, base64, a single-line `.jsonl`) is one "token", so a token-count
    /// budget happily emits a chunk the size of a whole file. Measured in this
    /// corpus: 1 039 923 chars in one chunk, whose vector can only ever encode
    /// the first 512 wordpieces.
    #[test]
    fn caps_a_whitespace_free_payload_at_the_char_ceiling() {
        let blob = "a".repeat(40_000);
        let chunks = chunk_sized(&blob, 640);
        assert!(
            chunks.len() > 20,
            "a 40k blob must be split, got {} chunk(s)",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.text.chars().count() <= MAX_CHUNK_CHARS,
                "chunk {} is {} chars, over the {} cap",
                c.index,
                c.text.chars().count(),
                MAX_CHUNK_CHARS
            );
        }
        // Nothing may be dropped at either end.
        assert!(chunks[0].text.starts_with(&"a".repeat(100)));
        assert!(chunks.last().unwrap().text.ends_with(&"a".repeat(100)));
        let covered: usize = chunks.iter().map(|c| c.text.chars().count()).sum();
        assert!(covered >= blob.chars().count());
    }

    /// Mixed real-world content: prose, newlines, and an embedded base64-ish
    /// run. Every emitted chunk must fit the model window.
    #[test]
    fn never_emits_a_chunk_wider_than_the_model_window() {
        let mut text = String::new();
        for i in 0..900 {
            text.push_str(&format!("line {i} some ordinary russian-ish words\n"));
            if i % 300 == 299 {
                text.push_str(&format!(
                    "data:image/png;base64,{}
",
                    "Q".repeat(9_000)
                ));
            }
        }
        let chunks = chunk_sized(&text, 640);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|c| c.text.chars().count() <= MAX_CHUNK_CHARS),
            "a chunk exceeded the cap: {:?}",
            chunks
                .iter()
                .map(|c| c.text.chars().count())
                .collect::<Vec<_>>()
        );
    }

    /// Behaviour preservation: a short document stays a single chunk with the
    /// text untouched apart from whitespace flattening.
    #[test]
    fn short_documents_stay_one_chunk() {
        let one = chunk_sized("one two three four five", 640);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].text, "one two three four five");
    }

    /// Both budgets must work, and which one binds depends on the corpus —
    /// asserted separately so a regression in either is visible.
    #[test]
    fn token_budget_binds_for_breviloquent_text_and_overlaps() {
        // 1-char words: 640 of them is ~1.3 k chars, under the cap.
        let text = (0..1_500)
            .map(|i| char::from(b'a' + (i % 26) as u8).to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let chunks = chunk_sized(&text, 640);
        assert_eq!(
            chunks[0].text.split_whitespace().count(),
            640,
            "token target must bind while it stays inside the char cap"
        );
        assert_shared_overlap(&chunks);
    }

    /// Ordinary words (~5 chars) put ~280 words past the 1.8 k cap, so the
    /// window closes early — that is the point of the cap: 640 such words
    /// would be ~700 wordpieces, and `encode()` truncates at 512, discarding
    /// the tail of every chunk while indexing it as if it were embedded.
    #[test]
    fn char_cap_binds_for_ordinary_prose_and_still_overlaps() {
        let text = (0..1_500)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let chunks = chunk_sized(&text, 640);
        assert!(chunks.len() > 4);
        for c in &chunks {
            let n = c.text.chars().count();
            assert!(n <= MAX_CHUNK_CHARS, "{n} chars over cap");
            if c.index + 1 < chunks.len() as u32 {
                assert!(
                    n > MAX_CHUNK_CHARS / 2,
                    "chunk {} only filled {n}/{} chars — the window is closing far too early",
                    c.index,
                    MAX_CHUNK_CHARS
                );
            }
        }
        assert_shared_overlap(&chunks);
    }

    /// Consecutive chunks must share [`OVERLAP_TOKENS`] tokens so a phrase
    /// that straddles a boundary stays findable in at least one of them.
    fn assert_shared_overlap(chunks: &[Chunk]) {
        let all: Vec<&str> = chunks[0].text.split_whitespace().collect();
        let tail = all[all.len().saturating_sub(OVERLAP_TOKENS)..].to_vec();
        let head: Vec<&str> = chunks[1]
            .text
            .split_whitespace()
            .take(OVERLAP_TOKENS)
            .collect();
        assert_eq!(
            tail, head,
            "consecutive chunks must share OVERLAP_TOKENS tokens"
        );
    }

    /// `chunk_index` stays dense and monotonic, because point ids are derived
    /// from it and gaps would orphan previously indexed points.
    #[test]
    fn chunk_indices_are_contiguous_from_zero() {
        let text = format!("{} {}", "word ".repeat(700), &"z".repeat(9_000));
        let chunks = chunk_sized(&text, 640);
        assert!(chunks.len() > 2);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.index as usize, i);
        }
    }
}
