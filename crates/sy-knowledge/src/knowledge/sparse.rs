//! In-house sparse encoder for hybrid retrieval (knowledge-retrieval-iter1
//! Step 4). Produces a deterministic term-frequency sparse vector
//! (`{indices, values}`) that maps directly onto qdrant's sparse-vector wire
//! form. No external crate, no corpus/IDF state in-process — qdrant applies
//! IDF server-side via the collection's `modifier: idf` (Step 3).
//!
//! Design contract (load-bearing):
//!   * tokenizer is unicode-aware so Cyrillic and alphanumerics survive
//!     (`X5`, `Магнит` must yield indices);
//!   * each distinct token hashes to a *stable* `u32` index — the same token
//!     yields the same index across calls and processes (fixed FNV-1a), so a
//!     re-index never shifts a token's column;
//!   * weights are a saturating term frequency (`tf * (K1 + 1) / (tf + K1)`),
//!     bounded and monotonic in count, with no global corpus state;
//!   * duplicate token indices collapse into one entry (qdrant sparse vectors
//!     require unique indices).

use std::collections::BTreeMap;

/// Saturating-tf shape constant. Mirrors BM25's `k1` so repeated tokens add
/// diminishing weight; IDF is intentionally absent (qdrant applies it).
const K1: f32 = 1.2;

/// FNV-1a 64-bit offset basis / prime, truncated to a stable 32-bit index.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A qdrant-shaped sparse vector: parallel `indices`/`values`, unique indices.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

/// Stable token → `u32` index via FNV-1a over the token's UTF-8 bytes.
/// Deterministic across calls and processes (no random seed).
fn token_index(token: &str) -> u32 {
    let mut hash = FNV_OFFSET;
    for byte in token.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Fold the 64-bit hash into 32 bits so the whole space is reachable.
    ((hash >> 32) ^ (hash & 0xffff_ffff)) as u32
}

/// Saturating term-frequency weight. Monotonic in `count`, bounded by `K1 + 1`.
fn tf_weight(count: u32) -> f32 {
    let tf = count as f32;
    tf * (K1 + 1.0) / (tf + K1)
}

/// Unicode-aware lowercase tokenizer: runs of alphanumeric characters,
/// everything else is a separator. Keeps Cyrillic and digits (`X5`, `Магнит`).
fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
}

/// Encode text into a deterministic term-frequency sparse vector. Same text in
/// → same `{indices, values}` out. Empty/whitespace-only text → empty vector.
pub fn encode(text: &str) -> SparseVector {
    let mut counts: BTreeMap<u32, u32> = BTreeMap::new();
    for token in tokenize(text) {
        *counts.entry(token_index(&token)).or_insert(0) += 1;
    }
    let mut indices = Vec::with_capacity(counts.len());
    let mut values = Vec::with_capacity(counts.len());
    for (index, count) in counts {
        indices.push(index);
        values.push(tf_weight(count));
    }
    SparseVector { indices, values }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_is_stable_for_fixed_text() {
        let text = "новый год X5 Магнит Лу";
        let a = encode(text);
        let b = encode(text);
        assert_eq!(a, b);
        assert!(!a.indices.is_empty());
    }

    #[test]
    fn rare_literal_token_appears_in_sparse_vector() {
        for token in ["X5", "Магнит"] {
            let v = encode(token);
            assert!(
                !v.indices.is_empty(),
                "token {token:?} produced no sparse indices"
            );
            assert_eq!(v.indices.len(), v.values.len());
        }
    }

    #[test]
    fn tokenizer_handles_cyrillic() {
        // A pure-Cyrillic phrase must tokenize into distinct stable indices.
        let v = encode("Магнит Пятёрочка Лента");
        assert_eq!(v.indices.len(), 3, "expected three distinct tokens");
        // Indices are unique (collapsed) and sorted (BTreeMap order).
        let mut sorted = v.indices.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, v.indices);
    }
}
