//! Fuzzy filename matcher backing the `sy file` plane's `/` filter.
//! Roadmap Step 25 (SPEC §3.3 item 7).
//!
//! [`matches`] takes a query string + a slice of [`Entry`]s and returns
//! the indices of the entries whose `name` fuzzy-matches the query,
//! sorted by descending [`nucleo::Matcher::fuzzy_match`] score (best
//! first). Stable: the same `(query, entries)` produces the same
//! `Vec<usize>` on every call — `Matcher` itself is stateful (it caches
//! its `Utf32` scratch buffers) but the score it computes is purely a
//! function of the input strings + the [`nucleo::Config`] knobs.
//!
//! The matcher uses `Config::DEFAULT.match_paths()` so directory
//! separators are honoured as boundary characters; case-insensitivity
//! rides on `Config::DEFAULT`'s `ignore_case = true` default. This is
//! the literal contract the roadmap Step 25 DoD bullet
//! `case_insensitive_by_default` rides on.

use nucleo::{Config, Matcher, Utf32Str};

use crate::file::state::Entry;

/// Return the indices of `entries` whose `name` fuzzy-matches `query`,
/// sorted by descending nucleo score (best matches first). Ties keep
/// the original `entries` order so a `(query, entries)` pair produces
/// a deterministic result across repeated calls — the Step 25
/// `matches_score_stable` test rides on this.
///
/// An empty `query` returns every index in `entries` (the journey-J7
/// "type `/`, see no rows yet" affordance — the filter starts open
/// with no narrowing).
pub fn matches(query: &str, entries: &[Entry]) -> Vec<usize> {
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    // `Utf32Str::new` needs a scratch buffer for non-ASCII; we reuse one
    // buffer per call so a long entries list pays one allocation.
    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(query, &mut needle_buf);
    let mut scored: Vec<(usize, u16)> = Vec::with_capacity(entries.len());
    for (idx, entry) in entries.iter().enumerate() {
        let mut hay_buf = Vec::new();
        let haystack = Utf32Str::new(&entry.name, &mut hay_buf);
        if let Some(score) = matcher.fuzzy_match(haystack, needle) {
            scored.push((idx, score));
        }
    }
    // Sort by descending score; stable sort preserves ties → input
    // order, which is what the `matches_score_stable` test asserts.
    scored.sort_by_key(|s| std::cmp::Reverse(s.1));
    scored.into_iter().map(|(idx, _)| idx).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::state::EntryKind;
    use std::time::SystemTime;

    /// Build a synthetic [`Entry`] with `name` set; the matcher only
    /// reads `name` so the rest of the fields collapse to defaults.
    fn entry(id: u64, name: &str) -> Entry {
        Entry {
            id,
            name: name.to_owned(),
            kind: EntryKind::File,
            size: 0,
            mtime: SystemTime::UNIX_EPOCH,
            is_symlink: false,
            broken_link: false,
            readable: true,
            mime_hint: None,
            symlink_target: None,
        }
    }

    /// Roadmap Step 25 pin: repeated calls with the same `(query,
    /// entries)` return the same `Vec<usize>`. `Matcher` holds internal
    /// scratch state so this asserts the score itself is a pure
    /// function of the inputs.
    #[test]
    fn matches_score_stable() {
        let entries = vec![
            entry(0, "Cargo.toml"),
            entry(1, "README.md"),
            entry(2, "src"),
            entry(3, "tests"),
        ];
        let a = matches("ar", &entries);
        let b = matches("ar", &entries);
        let c = matches("ar", &entries);
        assert_eq!(a, b, "matches must be deterministic across calls");
        assert_eq!(b, c, "third call must agree with the first two");
        assert!(
            !a.is_empty(),
            "query 'ar' must match at least one entry, got {a:?}"
        );
    }

    /// Roadmap Step 25 pin: `Config::DEFAULT.ignore_case = true` means
    /// `"readme"` matches `"README.md"`. The DoD bullet
    /// `case_insensitive_by_default` rides on this exact behaviour.
    #[test]
    fn case_insensitive_by_default() {
        let entries = vec![
            entry(0, "Cargo.toml"),
            entry(1, "src"),
            entry(2, "README.md"),
        ];
        let result = matches("readme", &entries);
        assert!(
            result.contains(&2),
            "lowercase 'readme' query must match 'README.md' (idx 2), got {result:?}"
        );
    }

    /// Empty query returns every index in original order — the
    /// "filter just opened" affordance the Step 25 commandbar paints
    /// before the user types anything.
    #[test]
    fn empty_query_returns_all_indices() {
        let entries = vec![entry(0, "a"), entry(1, "b"), entry(2, "c")];
        assert_eq!(matches("", &entries), vec![0, 1, 2]);
    }
}
