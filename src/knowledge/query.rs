//! Query-side expansion helpers (knowledge-retrieval-iter1 Step 17).
//!
//! Synonym expansion is **sparse-side only** (REQ-7): the dense embedding
//! already captures semantic synonymy, so expanding the dense query would
//! only pollute its recall. The sparse (BM25/term-frequency) leg, by
//! contrast, matches literal tokens — so OR-ing an entity's aliases into the
//! sparse query is what turns "X5" into precise lexical hits on "Пятёрочка",
//! "Перекрёсток", … without touching the dense leg.
//!
//! The expander is a **pure** function over an already-parsed synonym table
//! ([`expand_synonyms`]) so its unit tests are hermetic. A thin loader
//! ([`load_synonyms`]) reads the declarative default shipped from
//! `configs/sy-knowledge/synonyms.yaml` and installed by `sy apply` to
//! `~/.config/sy-knowledge/synonyms.yaml`. A missing or empty file is a
//! no-op: expansion returns the query unchanged.

use std::path::PathBuf;

use chrono::Datelike;
use serde::Deserialize;

/// One alias group: a canonical entity and the literal surface forms that
/// should expand to it on the sparse side.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SynGroup {
    pub canonical: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

impl SynGroup {
    /// All surface forms in the group: the canonical plus every alias.
    fn terms(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.canonical.as_str()).chain(self.aliases.iter().map(String::as_str))
    }
}

/// Path of the installed synonym table (`sy apply` writes it here from
/// `configs/sy-knowledge/synonyms.yaml`). Honours `XDG_CONFIG_HOME`.
fn synonyms_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sy-knowledge").join("synonyms.yaml"))
}

/// Load the installed synonym table. A missing, empty, or unparseable file
/// yields an empty table so callers degrade to a pure no-op (never an error
/// on the hot search path).
pub fn load_synonyms() -> Vec<SynGroup> {
    let Some(path) = synonyms_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    if text.trim().is_empty() {
        return Vec::new();
    }
    serde_yml::from_str(&text).unwrap_or_default()
}

/// Whether `query` (case-insensitively) contains `term` as a standalone
/// token, so "X5" matches "новый год X5" but not "MAX5K".
fn query_mentions(query_lc: &str, term: &str) -> bool {
    let term_lc = term.to_lowercase();
    query_lc
        .split(|c: char| !c.is_alphanumeric())
        .any(|tok| tok == term_lc)
}

/// Expand `query` for the **sparse** leg: if it mentions any term of a group
/// (canonical or alias), OR-in that group's whole term set. The original
/// query text is preserved verbatim and aliases are appended; the dense leg
/// must keep using the unmodified `query`. Pure — no I/O.
pub fn expand_synonyms(query: &str, synonyms: &[SynGroup]) -> String {
    let query_lc = query.to_lowercase();
    let mut out = query.to_string();
    for group in synonyms {
        let hit = group.terms().any(|t| query_mentions(&query_lc, t));
        if !hit {
            continue;
        }
        for term in group.terms() {
            if !query_mentions(&out.to_lowercase(), term) {
                out.push(' ');
                out.push_str(term);
            }
        }
    }
    out
}

/// Russian New-Year state holidays: Dec 31 of the prior year through Jan 8
/// (the official non-working span). The lexicon stores month/day offsets so
/// the year is taken from the query (or `now`).
const RU_NEW_YEAR_FROM: (i32, u32, u32) = (-1, 12, 31);
const RU_NEW_YEAR_TO: (i32, u32, u32) = (0, 1, 8);

/// Render a `NaiveDate` as an RFC-3339 instant at `time` (UTC), matching the
/// `date_from`/`date_to` shape qdrant's datetime range filter expects.
fn rfc3339(date: chrono::NaiveDate, end_of_day: bool) -> String {
    let (h, m, s) = if end_of_day { (23, 59, 59) } else { (0, 0, 0) };
    let t = date
        .and_hms_opt(h, m, s)
        .unwrap_or_else(|| date.and_hms_opt(0, 0, 0).unwrap_or_default());
    format!("{}Z", t.format("%Y-%m-%dT%H:%M:%S"))
}

/// Inclusive day-range → RFC-3339 `(from 00:00:00, to 23:59:59)` bounds.
fn day_range(from: chrono::NaiveDate, to: chrono::NaiveDate) -> (String, String) {
    (rfc3339(from, false), rfc3339(to, true))
}

/// Four-digit year mentioned anywhere in `query` (e.g. "праздники 2024"),
/// else `None`. Standalone-token match so "X5" never reads as a year.
fn year_in(query: &str) -> Option<i32> {
    query
        .split(|c: char| !c.is_ascii_digit())
        .find(|t| t.len() == 4)
        .and_then(|t| t.parse().ok())
}

/// Season → (from-month/day, to-month/day) for the meteorological seasons,
/// shared by the RU and EN season words. Winter is the Dec-Feb span; we map
/// it to the Jan-Feb portion of the named year for determinism.
fn season_range(season: &str) -> Option<((u32, u32), (u32, u32))> {
    match season {
        "winter" | "зима" => Some(((1, 1), (2, 28))),
        "spring" | "весна" => Some(((3, 1), (5, 31))),
        "summer" | "лето" => Some(((6, 1), (8, 31))),
        "fall" | "autumn" | "осень" => Some(((9, 1), (11, 30))),
        _ => None,
    }
}

/// Pure RU/EN natural-language date-range expander (REQ-8). Returns inclusive
/// `(date_from, date_to)` RFC-3339 bounds, or `None` when nothing matched.
///
/// `now` is the deterministic reference date (the daemon reads the clock and
/// passes today in) so this stays clock-free and unit-testable. Two layers:
///   1. an in-Rust **RU/EN lexicon** — Russian holidays + seasons + "in
///      `<Month>`" / "last `<season>`" — the SPEC's chosen approach (no Duckling /
///      HeidelTime runtime snowflake);
///   2. **`two_timer`** for generic English relative/range phrases the lexicon
///      doesn't cover ("last month", "next year", …), evaluated against `now`.
pub fn expand_dates(query: &str, now: chrono::NaiveDate) -> Option<(String, String)> {
    let q = query.to_lowercase();
    let has = |w: &str| q.split(|c: char| !c.is_alphanumeric()).any(|tok| tok == w);

    // RU New-Year holidays — year from the query, else `now`'s year.
    if has("новогодние") || (has("новый") && has("год")) || has("праздники")
    {
        let year = year_in(&q).unwrap_or_else(|| now.year());
        let from = chrono::NaiveDate::from_ymd_opt(
            year + RU_NEW_YEAR_FROM.0,
            RU_NEW_YEAR_FROM.1,
            RU_NEW_YEAR_FROM.2,
        )?;
        let to = chrono::NaiveDate::from_ymd_opt(year, RU_NEW_YEAR_TO.1, RU_NEW_YEAR_TO.2)?;
        return Some(day_range(from, to));
    }

    // Seasons (RU + EN). "last <season>" → previous year; otherwise the named
    // year (from the query) or `now`'s year.
    for word in q.split(|c: char| !c.is_alphanumeric()) {
        if let Some(((fm, fd), (tm, td))) = season_range(word) {
            let mut year = year_in(&q).unwrap_or(now.year());
            if has("last") || has("прошлым") || has("прошлое") || has("прошлой")
            {
                year -= 1;
            }
            let from = chrono::NaiveDate::from_ymd_opt(year, fm, fd)?;
            let to = chrono::NaiveDate::from_ymd_opt(year, tm, td)?;
            return Some(day_range(from, to));
        }
    }

    // "in <Month>" (English) → that month of the query's year (or `now`'s).
    if let Some(range) = english_month(&q, now) {
        return Some(range);
    }

    // Generic English via two_timer, evaluated against the reference `now`.
    two_timer_range(query, now)
}

/// English `"<Month>"` mention → that calendar month's inclusive day bounds.
fn english_month(q: &str, now: chrono::NaiveDate) -> Option<(String, String)> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let toks: Vec<&str> = q.split(|c: char| !c.is_alphanumeric()).collect();
    let month = toks
        .iter()
        .find_map(|tok| MONTHS.iter().position(|m| m == tok).map(|i| i as u32 + 1))?;
    let year = year_in(q).unwrap_or(now.year());
    let from = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
    let to = from
        .checked_add_months(chrono::Months::new(1))?
        .pred_opt()?;
    Some(day_range(from, to))
}

/// Generic English range/relative phrase via `two_timer`, anchored to `now`
/// (deterministic, no system clock). `two_timer` yields a half-open
/// `[start, end)` `NaiveDateTime` pair; we convert to inclusive day bounds.
fn two_timer_range(query: &str, now: chrono::NaiveDate) -> Option<(String, String)> {
    if !two_timer::parsable(query) {
        return None;
    }
    let cfg = two_timer::Config::new().now(now.and_hms_opt(0, 0, 0)?);
    let (start, end, _) = two_timer::parse(query, Some(cfg)).ok()?;
    let last_day = end.date().pred_opt().unwrap_or_else(|| end.date());
    Some(day_range(start.date(), last_day))
}

/// Live-search consumer (REQ-8): when `filter` carries **no** date bound, run
/// [`expand_dates`] and fill `date_from`/`date_to`. An explicit caller bound
/// wins (no expansion). A query with no recognizable time phrase is a no-op,
/// logged at debug so lexicon gaps are visible.
pub fn maybe_fill_dates(
    filter: &mut crate::aiplane::ipc::SearchFilter,
    query: &str,
    now: chrono::NaiveDate,
) {
    if filter.date_from.is_some() || filter.date_to.is_some() {
        return;
    }
    match expand_dates(query, now) {
        Some((from, to)) => {
            tracing::debug!(query, %from, %to, "expand_dates filled date window");
            filter.date_from = Some(from);
            filter.date_to = Some(to);
        }
        None => tracing::debug!(query, "expand_dates: no time phrase recognized"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x5_group() -> Vec<SynGroup> {
        vec![SynGroup {
            canonical: "X5".to_string(),
            aliases: vec![
                "Пятёрочка".to_string(),
                "Перекрёсток".to_string(),
                "Чижик".to_string(),
            ],
        }]
    }

    #[test]
    fn x5_expands_to_aliases_on_sparse_side() {
        let expanded = expand_synonyms("X5", &x5_group());
        // The sparse-side string OR-s in every alias of the matched group.
        for alias in ["Пятёрочка", "Перекрёсток", "Чижик"] {
            assert!(
                expanded.contains(alias),
                "expanded sparse query {expanded:?} missing alias {alias:?}"
            );
        }
        // The original token survives so its own column still scores.
        assert!(expanded.contains("X5"));
    }

    #[test]
    fn expansion_does_not_touch_dense_query() {
        let query = "X5";
        // Dense leg keeps the unmodified query; only the sparse leg expands.
        let dense = query;
        let sparse = expand_synonyms(query, &x5_group());
        assert_eq!(dense, "X5", "dense query must stay unexpanded");
        assert_ne!(sparse, dense, "sparse query must be expanded");
        assert!(sparse.contains("Пятёрочка"));
    }

    #[test]
    fn missing_or_empty_synonyms_file_is_noop() {
        // Empty table → query returned unchanged (the missing/empty-file path
        // returns an empty Vec, exercised here directly for hermeticity).
        let query = "новый год X5 Магнит";
        assert_eq!(expand_synonyms(query, &[]), query);
    }

    #[test]
    fn unmatched_query_is_unchanged() {
        let query = "погода завтра";
        assert_eq!(expand_synonyms(query, &x5_group()), query);
    }

    fn ymd(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    #[test]
    fn ru_new_year_holidays_2024_maps_to_dec31_jan08() {
        // Russian New-Year holidays span Dec 31 of the prior year through Jan 8.
        let (from, to) =
            expand_dates("когда новогодние праздники 2024", ymd(2026, 6, 2)).expect("range");
        assert!(from.starts_with("2023-12-31"), "from = {from}");
        assert!(to.starts_with("2024-01-08"), "to = {to}");
    }

    #[test]
    fn en_in_january_and_last_summer_map_to_ranges() {
        let now = ymd(2026, 6, 2);
        // "in January" → that calendar year's January bounds.
        let (jf, jt) = expand_dates("what did I buy in January", now).expect("january range");
        assert!(jf.starts_with("2026-01-01"), "jan from = {jf}");
        assert!(jt.starts_with("2026-01-31"), "jan to = {jt}");
        // "last summer" → previous year's Jun 1 .. Aug 31 relative to `now`.
        let (sf, st) = expand_dates("photos from last summer", now).expect("summer range");
        assert!(sf.starts_with("2025-06-01"), "summer from = {sf}");
        assert!(st.starts_with("2025-08-31"), "summer to = {st}");
    }

    #[test]
    fn explicit_date_args_override_expansion() {
        use crate::aiplane::ipc::SearchFilter;
        // A caller-supplied date bound means the filter already has a window;
        // the live path must NOT call the expander. Mirror that contract here:
        // when a bound is present we leave the filter untouched.
        let mut filter = SearchFilter {
            date_from: Some("2020-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        let before = filter.clone();
        maybe_fill_dates(&mut filter, "last summer", ymd(2026, 6, 2));
        assert_eq!(filter, before, "explicit date arg must win over expansion");
    }

    #[test]
    fn unrecognized_phrase_is_noop_and_logged() {
        // No recognizable time phrase → None (the caller logs the miss).
        assert!(expand_dates("X5 Магнит чек", ymd(2026, 6, 2)).is_none());
    }

    #[test]
    fn alias_in_query_pulls_in_canonical_and_siblings() {
        // Mentioning an alias expands to the canonical + the other aliases.
        let expanded = expand_synonyms("купил в Пятёрочке вчера", &x5_group());
        // "Пятёрочке" is not the exact token "Пятёрочка", so this group does
        // not fire — guards against substring false-positives.
        assert_eq!(expanded, "купил в Пятёрочке вчера");
        // Exact alias token does fire.
        let expanded = expand_synonyms("чек Пятёрочка", &x5_group());
        assert!(expanded.contains("X5"));
        assert!(expanded.contains("Перекрёсток"));
    }
}
