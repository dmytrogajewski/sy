//! Thin synchronous Qdrant REST client.
//!
//! We use blocking reqwest because all callers (CLI subcommands, the
//! daemon's index loop, the MCP server) are happy to block — there's no
//! UI thread to keep responsive. Endpoints we hit:
//!
//!   PUT    /collections/{name}              create
//!   GET    /collections/{name}              probe existence
//!   PUT    /collections/{name}/points       upsert
//!   POST   /collections/{name}/points/delete
//!   POST   /collections/{name}/points/search

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::sparse::SparseVector;
use super::{exit, COLLECTION, DENSE_VECTOR, QDRANT_PORT, SPARSE_VECTOR, VECTOR_DIM};

pub fn base_url() -> String {
    format!("http://127.0.0.1:{QDRANT_PORT}")
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?)
}

fn unreachable_error(e: anyhow::Error) -> anyhow::Error {
    super::KnowledgeError {
        code: exit::QDRANT_UNREACHABLE,
        msg: format!(
            "qdrant unreachable on {} — is the daemon running? ({e})",
            base_url()
        ),
    }
    .into()
}

/// Return Ok(true) if qdrant responds on /readyz within ~1 s.
pub fn is_ready() -> bool {
    let c = match client() {
        Ok(c) => c,
        Err(_) => return false,
    };
    c.get(format!("{}/readyz", base_url()))
        .timeout(Duration::from_secs(1))
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Wait up to `timeout_secs` for qdrant to become ready.
pub fn wait_ready(timeout_secs: u64) -> Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < timeout_secs {
        if is_ready() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(unreachable_error(anyhow::anyhow!(
        "timed out after {timeout_secs}s"
    )))
}

/// Create the `sy_knowledge` collection if it doesn't exist. Also
/// (idempotently) creates the payload indexes the dropdown / facet
/// queries depend on — `tags` (keyword) so per-tag counts come back in
/// milliseconds instead of a full scan.
pub fn ensure_collection() -> Result<()> {
    let c = client()?;
    let url = format!("{}/collections/{}", base_url(), COLLECTION);
    let exists = c
        .get(&url)
        .send()
        .map(|r| r.status().is_success())
        .map_err(|e| unreachable_error(e.into()))?;
    if !exists {
        let body = collection_create_body();
        let resp = c
            .put(&url)
            .json(&body)
            .send()
            .context("create collection")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let txt = resp.text().unwrap_or_default();
            anyhow::bail!("qdrant: create collection {COLLECTION} failed ({status}): {txt}");
        }
    }
    // Payload indexes — best-effort, created before any ingest so the
    // filterable-HNSW edges form. Re-creating an existing index is a 200 in
    // qdrant 1.x, so this is safe to call on every startup.
    for (field, schema) in PAYLOAD_INDEXES {
        let _ = ensure_payload_index(field, schema);
    }
    Ok(())
}

/// Pure construction of the v2 collection-create request body. Named `dense`
/// (Cosine, `VECTOR_DIM`) + a `sparse` vector with the `idf` modifier so
/// qdrant applies IDF server-side (Step 4 writes the sparse weights). Split out
/// so the schema shape is unit-testable without a live qdrant.
fn collection_create_body() -> Value {
    json!({
        "vectors": {
            DENSE_VECTOR: { "size": VECTOR_DIM, "distance": "Cosine" }
        },
        "sparse_vectors": {
            SPARSE_VECTOR: { "modifier": "idf" }
        }
    })
}

/// Payload indexes the collection must carry before ingest. `datetime`
/// (qdrant ≥1.8) keeps RFC 3339 dates native; `bool` (qdrant ≥1.4) indexes
/// `has_media`; the rest are keyword facets/filters.
const PAYLOAD_INDEXES: &[(&str, &str)] = &[
    ("tags", "keyword"),
    ("date", "datetime"),
    ("kind", "keyword"),
    ("source_name", "keyword"),
    ("from", "keyword"),
    ("has_media", "bool"),
];

/// Pure construction of a qdrant create-index request body. Split out so the
/// request shape is unit-testable without a live qdrant.
fn index_create_body(field_name: &str, field_schema: &str) -> Value {
    json!({
        "field_name": field_name,
        "field_schema": field_schema,
    })
}

/// Idempotently create a payload index on `field_name`. `field_schema`
/// is the qdrant payload schema string (`"keyword"`, `"integer"`, …).
pub fn ensure_payload_index(field_name: &str, field_schema: &str) -> Result<()> {
    let c = client()?;
    let url = format!("{}/collections/{}/index", base_url(), COLLECTION);
    let body = index_create_body(field_name, field_schema);
    let resp = c.put(&url).json(&body).send().context("create index")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().unwrap_or_default();
        anyhow::bail!("qdrant: create index {field_name} failed ({status}): {txt}");
    }
    Ok(())
}

/// Decide, from a parsed qdrant collection-info body, whether the live
/// collection predates the v2 named-vector schema and must be migrated.
///
/// A pre-v2 collection stores a single *unnamed* dense vector, so
/// `config.params.vectors` is a flat `{size, distance}` object with no
/// named `dense` key. A v2 collection nests the vector under `dense`. Pure
/// so the daemon-startup migration trigger is unit-testable without a live
/// qdrant.
pub fn schema_is_pre_v2(collection_info: &Value) -> bool {
    let vectors = &collection_info["result"]["config"]["params"]["vectors"];
    // v2 ⟺ a named `dense` vector config exists; anything else (the old flat
    // `{size,distance}` shape, or a missing/null vectors block) is pre-v2.
    vectors.get(DENSE_VECTOR).map(Value::is_object) != Some(true)
}

/// Fetch the live collection-info body, returning `None` when the collection
/// does not exist (a fresh collection is created at v2, so no migration) or
/// qdrant is unreachable.
pub fn collection_info() -> Option<Value> {
    let c = client().ok()?;
    let url = format!("{}/collections/{}", base_url(), COLLECTION);
    let resp = c.get(&url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().ok()
}

/// Minimum qdrant version the hybrid Universal Query needs: configurable
/// RRF `k` (`query.rrf.k = 60`, Step 5) only lands in qdrant ≥ 1.16. Below
/// this, qdrant silently ignores `k` (defaults to 2) and hybrid search
/// regresses. Asserted at daemon start + surfaced by `sy doctor`.
pub const MIN_HYBRID_VERSION: (u32, u32) = (1, 16);

/// Parse a qdrant root `GET /` body into a `(major, minor)` version.
/// The body looks like
/// `{"title":"qdrant - vector search engine","version":"1.18.1",...}`.
/// Returns `None` for non-JSON, a missing/empty `version`, or a value
/// without two numeric leading components. Pure so the parse is
/// unit-testable without a live qdrant.
pub fn parse_version(body: &str) -> Option<(u32, u32)> {
    let v: Value = serde_json::from_str(body).ok()?;
    let ver = v.get("version")?.as_str()?;
    let mut parts = ver.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// True iff `v` is at least `min`, compared as `(major, minor)` tuples.
pub fn meets_min_version(v: (u32, u32), min: (u32, u32)) -> bool {
    v >= min
}

/// Fetch the live qdrant `(major, minor)` version from `GET /`. Returns
/// `None` when qdrant is unreachable or the body is unparseable. Best-effort
/// (1 s timeout) so callers degrade to "unknown" rather than blocking.
pub fn server_version() -> Option<(u32, u32)> {
    let c = client().ok()?;
    let resp = c
        .get(format!("{}/", base_url()))
        .timeout(Duration::from_secs(1))
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    parse_version(&resp.text().ok()?)
}

/// Drop and recreate the collection (used by `sy knowledge sync --yes`).
pub fn recreate_collection() -> Result<()> {
    let c = client()?;
    let url = format!("{}/collections/{}", base_url(), COLLECTION);
    let _ = c.delete(&url).send();
    ensure_collection()
}

#[derive(Debug, Clone, Serialize)]
pub struct Point {
    pub id: String,
    pub vector: Vec<f32>,
    /// Term-frequency sparse vector for hybrid retrieval (Step 4). Written
    /// alongside the named `dense` vector; `idf` is applied server-side.
    #[serde(skip)]
    pub sparse: SparseVector,
    pub payload: PointPayload,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PointPayload {
    pub source: String,
    pub file_path: String,
    pub chunk_index: u32,
    pub chunk_text: String,
    pub file_mtime: u64,
    pub content_hash: String,
    /// Free-form labels supplied by `qdr.toml` (`tags = [...]`). Empty for
    /// chunks that came from a non-manifest source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// `SourceKind` kebab string (Step 1). Drives the default-scope
    /// transcript exclusion and the `kind` keyword filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Stable source name from the registry (keyword filter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    /// RFC 3339 datetime string for the record, when derivable. Indexed as
    /// qdrant's native `datetime` type — never an epoch int.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Author / sender (keyword filter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Whether the record points at media (bool filter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_media: Option<bool>,
    /// Source-native message id (e.g. Telegram).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<i64>,
    /// Source-native id of the message this one replies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_id: Option<i64>,
    /// Model that produced a Claude-transcript turn (keyword filter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Project id for a Claude-transcript turn (keyword filter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// Pure construction of the upsert request body. Targets the named `dense`
/// vector plus the named `sparse` vector (v2 schema) —
/// `vector: { "dense": [...], "sparse": { "indices": [..], "values": [..] } }`.
/// A point with an empty sparse vector still serializes the (empty) named
/// vector; qdrant tolerates it.
pub(crate) fn upsert_body(points: &[Point]) -> Value {
    json!({
        "points": points.iter().map(|p| json!({
            "id": p.id,
            "vector": {
                DENSE_VECTOR: p.vector,
                SPARSE_VECTOR: {
                    "indices": p.sparse.indices,
                    "values": p.sparse.values,
                },
            },
            "payload": p.payload,
        })).collect::<Vec<_>>()
    })
}

/// Upsert a batch of points. Caller is responsible for batching.
pub fn upsert(points: &[Point]) -> Result<()> {
    if points.is_empty() {
        return Ok(());
    }
    let c = client()?;
    let url = format!("{}/collections/{}/points?wait=true", base_url(), COLLECTION);
    let body = upsert_body(points);
    let resp = c.put(&url).json(&body).send().context("upsert")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().unwrap_or_default();
        anyhow::bail!("qdrant: upsert failed ({status}): {txt}");
    }
    Ok(())
}

/// Delete every point whose `payload.source` matches the given root. Used
/// when a manifest folder gets disabled or its `qdr.toml` removed — we want
/// the points gone even if the daemon's stale-cleanup hasn't run yet.
pub fn delete_by_source(source: &str) -> Result<()> {
    let c = client()?;
    let url = format!(
        "{}/collections/{}/points/delete?wait=true",
        base_url(),
        COLLECTION
    );
    let body = json!({
        "filter": {
            "must": [{
                "key": "source",
                "match": { "value": source }
            }]
        }
    });
    let resp = c
        .post(&url)
        .json(&body)
        .send()
        .context("delete by source")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().unwrap_or_default();
        anyhow::bail!("qdrant: delete_by_source failed ({status}): {txt}");
    }
    Ok(())
}

/// Delete points by their ids.
pub fn delete_points(ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let c = client()?;
    let url = format!(
        "{}/collections/{}/points/delete?wait=true",
        base_url(),
        COLLECTION
    );
    let body = json!({ "points": ids });
    let resp = c.post(&url).json(&body).send().context("delete points")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().unwrap_or_default();
        anyhow::bail!("qdrant: delete failed ({status}): {txt}");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct SearchHit {
    pub score: f32,
    pub payload: PointPayload,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    result: Vec<SearchHit>,
}

/// Pure construction of the dense-search request body. Targets the named
/// `dense` vector (v2 schema) via qdrant's `{ name, vector }` form so the
/// dense-only behaviour is preserved across the named-vector migration.
fn search_body(vector: &[f32], limit: usize, filter: Option<&Value>) -> Value {
    let mut body = json!({
        "vector": { "name": DENSE_VECTOR, "vector": vector },
        "limit": limit,
        "with_payload": true,
    });
    if let Some(f) = filter {
        body["filter"] = f.clone();
    }
    body
}

/// Pure construction of the hybrid Universal-Query request body. Two
/// `prefetch` legs — a dense float-array leg (`using: "dense"`) and a sparse
/// `{indices, values}` leg (`using: "sparse"`) — fused by a top-level
/// `query: { rrf: { k: 60 } }`. **`k` is set explicitly**: qdrant's built-in
/// RRF default is `k=2`, not the textbook 60 (SPEC §2 "RRF k gotcha"). The
/// `filter` (when present) is a qdrant `Filter` JSON object applied to both
/// prefetch legs as a pre-filter; Step 7 extends this with the structured
/// `SearchFilter` compiler. Split out so the JSON shape is unit-testable
/// without a live qdrant.
fn hybrid_query_body(
    dense: &[f32],
    sparse: &SparseVector,
    filter: Option<&Value>,
    limit: usize,
) -> Value {
    let mut dense_leg = json!({
        "query": dense,
        "using": DENSE_VECTOR,
        "limit": limit,
    });
    let mut sparse_leg = json!({
        "query": { "indices": sparse.indices, "values": sparse.values },
        "using": SPARSE_VECTOR,
        "limit": limit,
    });
    if let Some(f) = filter {
        dense_leg["filter"] = f.clone();
        sparse_leg["filter"] = f.clone();
    }
    json!({
        "prefetch": [dense_leg, sparse_leg],
        "query": { "rrf": { "k": RRF_K } },
        "limit": limit,
        "with_payload": true,
    })
}

/// Explicit Reciprocal-Rank-Fusion `k`. qdrant's built-in default is 2, which
/// is too top-heavy; the textbook / SPEC-mandated value is 60. Requires qdrant
/// ≥ 1.16 (configurable `k` landed there).
const RRF_K: u32 = 60;

#[derive(Debug, Deserialize)]
struct QueryResponse {
    result: QueryResult,
}
#[derive(Debug, Deserialize)]
struct QueryResult {
    points: Vec<SearchHit>,
}

/// Compile a structured [`SearchFilter`](crate::aiplane::ipc::SearchFilter)
/// (plus an optional `file_path` prefix
/// text-match, preserving the Step 5 behaviour) into a qdrant `Filter` JSON
/// object. Returns `None` when nothing constrains the search (empty filter and
/// no prefix). Field → clause shapes are verified against SPEC §2:
///   * `date_from`/`date_to` → `must { key: "date", range: { gte, lte } }`
///   * `from`/`kind`         → `must { key, match: { any: [..] } }` (MatchAny)
///   * `include_sources`     → `must { key: "source_name", match: { any } }`
///   * `exclude_sources`     → `must_not { key: "source_name", match: { any } }`
///   * `exclude_kinds`       → `must_not { key: "kind", match: { any } }`
///   * `prefix`              → `must { key: "file_path", match: { text } }`
///
/// `must`/`must_not` arrays are omitted when empty so the body stays compact.
pub fn build_filter(
    filter: &crate::aiplane::ipc::SearchFilter,
    prefix: Option<&str>,
) -> Option<Value> {
    let mut must: Vec<Value> = Vec::new();
    let mut must_not: Vec<Value> = Vec::new();

    if filter.date_from.is_some() || filter.date_to.is_some() {
        let mut range = serde_json::Map::new();
        if let Some(gte) = &filter.date_from {
            range.insert("gte".into(), json!(gte));
        }
        if let Some(lte) = &filter.date_to {
            range.insert("lte".into(), json!(lte));
        }
        must.push(json!({ "key": "date", "range": Value::Object(range) }));
    }
    let any = |key: &str, vals: &[String]| json!({ "key": key, "match": { "any": vals } });
    if !filter.from.is_empty() {
        must.push(any("from", &filter.from));
    }
    if !filter.kind.is_empty() {
        must.push(any("kind", &filter.kind));
    }
    if !filter.include_sources.is_empty() {
        must.push(any("source_name", &filter.include_sources));
    }
    if let Some(p) = prefix {
        must.push(json!({ "key": "file_path", "match": { "text": p } }));
    }
    if !filter.exclude_sources.is_empty() {
        must_not.push(any("source_name", &filter.exclude_sources));
    }
    if !filter.exclude_kinds.is_empty() {
        must_not.push(any("kind", &filter.exclude_kinds));
    }

    if must.is_empty() && must_not.is_empty() {
        return None;
    }
    let mut out = serde_json::Map::new();
    if !must.is_empty() {
        out.insert("must".into(), Value::Array(must));
    }
    if !must_not.is_empty() {
        out.insert("must_not".into(), Value::Array(must_not));
    }
    Some(Value::Object(out))
}

/// Hybrid retrieval via the qdrant Universal Query API: a dense prefetch leg
/// and a sparse prefetch leg, fused by RRF (`k=60`). `filter` (a qdrant
/// `Filter` JSON object) pre-filters both legs — Step 7 extends this with the
/// structured `SearchFilter` compiler. Returns the fused top-`limit`
/// candidates for the downstream rerank stage.
pub fn query_hybrid(
    dense: &[f32],
    sparse: &SparseVector,
    filter: Option<&Value>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let c = client()?;
    let url = format!("{}/collections/{}/points/query", base_url(), COLLECTION);
    let body = hybrid_query_body(dense, sparse, filter, limit);
    let resp = c
        .post(&url)
        .json(&body)
        .send()
        .map_err(|e| unreachable_error(e.into()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().unwrap_or_default();
        anyhow::bail!("qdrant: hybrid query failed ({status}): {txt}");
    }
    let parsed: QueryResponse = resp.json().context("parse qdrant query response")?;
    Ok(parsed.result.points)
}

/// Dense vector search applying a pre-built qdrant `Filter` (the structured
/// [`build_filter`] output). This powers the embed-only `Req::Search` path so
/// it honours `exclude_kinds` / `from` / date bounds exactly like the rerank
/// path, instead of silently dropping the filter.
pub fn search_with_filter(
    vector: &[f32],
    limit: usize,
    filter: Option<&Value>,
) -> Result<Vec<SearchHit>> {
    let c = client()?;
    let url = format!("{}/collections/{}/points/search", base_url(), COLLECTION);
    let body = search_body(vector, limit, filter);
    let resp = c
        .post(&url)
        .json(&body)
        .send()
        .map_err(|e| unreachable_error(e.into()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().unwrap_or_default();
        anyhow::bail!("qdrant: search failed ({status}): {txt}");
    }
    let parsed: SearchResponse = resp.json().context("parse qdrant response")?;
    Ok(parsed.result)
}

/// Pure construction of the points-retrieve request body for a single id.
/// `with_payload: true` returns the full (uncapped) `chunk_text`; no vectors.
/// Split out so the JSON shape is unit-testable without a live qdrant.
pub(crate) fn retrieve_body(chunk_id: &str) -> Value {
    json!({
        "ids": [chunk_id],
        "with_payload": true,
        "with_vector": false,
    })
}

#[derive(Debug, Deserialize)]
struct RetrieveResponse {
    result: Vec<RetrievedPoint>,
}

#[derive(Debug, Deserialize)]
struct RetrievedPoint {
    payload: PointPayload,
}

/// REQ-10 fetch-by-id: resolve a single point's full payload (with its
/// uncapped `chunk_text`) by id. `Ok(None)` when no point matches. The id
/// is the blake3-derived `chunk::point_id` a search result carries.
pub fn get_point(chunk_id: &str) -> Result<Option<PointPayload>> {
    let c = client()?;
    let url = format!("{}/collections/{}/points", base_url(), COLLECTION);
    let resp = c
        .post(&url)
        .json(&retrieve_body(chunk_id))
        .send()
        .map_err(|e| unreachable_error(e.into()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().unwrap_or_default();
        anyhow::bail!("qdrant: retrieve failed ({status}): {txt}");
    }
    let parsed: RetrieveResponse = resp.json().context("parse qdrant retrieve response")?;
    Ok(parsed.result.into_iter().next().map(|p| p.payload))
}

#[derive(Debug, Deserialize)]
struct CountResponse {
    result: CountResult,
}
#[derive(Debug, Deserialize)]
struct CountResult {
    count: u64,
}

pub fn point_count() -> Result<u64> {
    let c = client()?;
    let url = format!("{}/collections/{}/points/count", base_url(), COLLECTION);
    let resp = c
        .post(&url)
        .json(&json!({"exact": true}))
        .send()
        .map_err(|e| unreachable_error(e.into()))?;
    if !resp.status().is_success() {
        return Ok(0);
    }
    let r: CountResponse = resp.json().context("parse count response")?;
    Ok(r.result.count)
}

#[derive(Debug, Deserialize)]
struct FacetResponse {
    result: FacetResult,
}
#[derive(Debug, Deserialize)]
struct FacetResult {
    hits: Vec<FacetHit>,
}
#[derive(Debug, Deserialize)]
struct FacetHit {
    value: Value,
    count: u64,
}

/// Return up to `limit` (value, count) pairs for the `tags` payload key.
/// Requires the `tags` keyword index (created by `ensure_collection`).
pub fn facet_tags(limit: usize) -> Result<Vec<(String, u64)>> {
    let c = client()?;
    let url = format!("{}/collections/{}/facet", base_url(), COLLECTION);
    let resp = c
        .post(&url)
        .json(&json!({"key": "tags", "limit": limit, "exact": true}))
        .send()
        .map_err(|e| unreachable_error(e.into()))?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let r: FacetResponse = resp.json().context("parse facet response")?;
    Ok(r.result
        .hits
        .into_iter()
        .filter_map(|h| h.value.as_str().map(|s| (s.to_string(), h.count)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_serializes_optional_metadata() {
        let p = PointPayload {
            source: "s".into(),
            file_path: "f".into(),
            chunk_index: 0,
            chunk_text: "t".into(),
            file_mtime: 0,
            content_hash: "h".into(),
            tags: vec![],
            kind: Some("telegram".into()),
            source_name: Some("tg".into()),
            date: Some("2024-01-02T03:04:05Z".into()),
            from: Some("alice".into()),
            has_media: Some(true),
            message_id: Some(42),
            reply_to_id: Some(7),
            model: None,
            project_id: None,
        };
        let v = serde_json::to_value(&p).expect("serialize");
        assert_eq!(v["kind"], "telegram");
        assert_eq!(v["source_name"], "tg");
        assert_eq!(v["date"], "2024-01-02T03:04:05Z");
        assert_eq!(v["from"], "alice");
        assert_eq!(v["has_media"], true);
        assert_eq!(v["message_id"], 42);
        assert_eq!(v["reply_to_id"], 7);

        // All optional: absent fields don't appear in the payload.
        let bare = serde_json::to_value(PointPayload::default()).expect("serialize");
        assert!(bare.get("kind").is_none());
        assert!(bare.get("date").is_none());
        assert!(bare.get("has_media").is_none());
        assert!(bare.get("message_id").is_none());
        assert!(bare.get("reply_to_id").is_none());
    }

    #[test]
    fn ensure_collection_v2_declares_named_dense_and_sparse() {
        let body = collection_create_body();
        // Named dense vector: Cosine, VECTOR_DIM.
        assert_eq!(body["vectors"]["dense"]["size"], VECTOR_DIM);
        assert_eq!(body["vectors"]["dense"]["distance"], "Cosine");
        // Sparse vector declared with server-side IDF modifier (written in
        // Step 4 — qdrant tolerates points missing the named sparse vector).
        assert!(body["sparse_vectors"]["sparse"].is_object());
        assert_eq!(body["sparse_vectors"]["sparse"]["modifier"], "idf");
        // The old top-level unnamed `{size,distance}` shape is gone.
        assert!(body["vectors"]["size"].is_null());
    }

    #[test]
    fn upsert_and_search_target_the_named_dense_vector() {
        let p = Point {
            id: "p1".into(),
            vector: vec![0.5, 0.25, 0.125],
            sparse: SparseVector {
                indices: vec![7, 42],
                values: vec![1.0, 2.0],
            },
            payload: PointPayload::default(),
        };
        let up = upsert_body(std::slice::from_ref(&p));
        assert_eq!(
            up["points"][0]["vector"]["dense"],
            json!([0.5, 0.25, 0.125])
        );
        // Named sparse vector rides alongside the dense one.
        assert_eq!(
            up["points"][0]["vector"]["sparse"]["indices"],
            json!([7, 42])
        );
        assert_eq!(
            up["points"][0]["vector"]["sparse"]["values"],
            json!([1.0, 2.0])
        );
        assert!(up["points"][0]["vector"].get("size").is_none());

        let s = search_body(&[0.5, 0.25], 7, None);
        assert_eq!(s["vector"]["name"], "dense");
        assert_eq!(s["vector"]["vector"], json!([0.5, 0.25]));
        assert_eq!(s["limit"], 7);
        assert!(s.get("filter").is_none());
    }

    #[test]
    fn search_body_embeds_the_structured_filter() {
        // Regression: the embed-only Search path used to drop the filter, so
        // `exclude_kinds` (the self-poisoning default-exclude) never reached
        // qdrant. The body must carry whatever `build_filter` produced.
        let f = crate::aiplane::ipc::SearchFilter {
            exclude_kinds: vec!["agent-history".into()],
            ..Default::default()
        };
        let pre = build_filter(&f, None).expect("non-empty filter");
        let body = search_body(&[0.1, 0.2], 5, Some(&pre));
        assert_eq!(
            body["filter"]["must_not"][0]["match"]["any"],
            json!(["agent-history"])
        );
    }

    #[test]
    fn hybrid_query_body_has_two_prefetch_legs_and_rrf_k60() {
        let dense = vec![0.5f32, 0.25, 0.125];
        let sparse = SparseVector {
            indices: vec![7, 42],
            values: vec![1.0, 2.0],
        };
        let body = hybrid_query_body(&dense, &sparse, None, 8);

        // Two prefetch legs: one dense, one sparse, each `using` the named vector.
        let legs = body["prefetch"].as_array().expect("prefetch legs");
        assert_eq!(legs.len(), 2);
        let dense_leg = legs
            .iter()
            .find(|l| l["using"] == "dense")
            .expect("dense leg");
        assert_eq!(dense_leg["query"], json!([0.5, 0.25, 0.125]));
        let sparse_leg = legs
            .iter()
            .find(|l| l["using"] == "sparse")
            .expect("sparse leg");
        assert_eq!(sparse_leg["query"]["indices"], json!([7, 42]));
        assert_eq!(sparse_leg["query"]["values"], json!([1.0, 2.0]));

        // Top-level RRF fusion with the explicit k=60 (qdrant default is 2).
        assert_eq!(body["query"]["rrf"]["k"], 60);
        assert_eq!(body["limit"], 8);
        assert_eq!(body["with_payload"], true);
        // No filter passed → no `filter` key (Step 7 plumbs real filters).
        assert!(body.get("filter").is_none());
    }

    #[test]
    fn retrieve_body_requests_payload_for_the_id_without_vectors() {
        let body = retrieve_body("deadbeef-0000-1111-2222-333344445555");
        assert_eq!(body["ids"], json!(["deadbeef-0000-1111-2222-333344445555"]));
        assert_eq!(body["with_payload"], true);
        // The fetch returns the full uncapped chunk_text from the payload; the
        // (large) vectors are not needed and must not be pulled back.
        assert_eq!(body["with_vector"], false);
    }

    #[test]
    fn filter_builds_datetime_range_and_keyword_match() {
        use crate::aiplane::ipc::SearchFilter;
        let f = SearchFilter {
            date_from: Some("2024-01-01T00:00:00Z".into()),
            date_to: Some("2024-12-31T23:59:59Z".into()),
            from: vec!["alice".into(), "bob".into()],
            kind: vec!["telegram".into()],
            include_sources: vec!["tg".into()],
            exclude_sources: vec!["claude-transcripts".into()],
            ..Default::default()
        };
        let v = build_filter(&f, None).expect("non-empty filter");

        let must = v["must"].as_array().expect("must clauses");
        // datetime range on `date` carries both bounds.
        let range = must
            .iter()
            .find(|c| c["key"] == "date")
            .expect("date clause");
        assert_eq!(range["range"]["gte"], "2024-01-01T00:00:00Z");
        assert_eq!(range["range"]["lte"], "2024-12-31T23:59:59Z");
        // `from` → MatchAny.
        let from = must
            .iter()
            .find(|c| c["key"] == "from")
            .expect("from clause");
        assert_eq!(from["match"]["any"], json!(["alice", "bob"]));
        // `kind` → MatchAny.
        let kind = must
            .iter()
            .find(|c| c["key"] == "kind")
            .expect("kind clause");
        assert_eq!(kind["match"]["any"], json!(["telegram"]));
        // `include_sources` → must MatchAny on `source_name`.
        let inc = must
            .iter()
            .find(|c| c["key"] == "source_name")
            .expect("include clause");
        assert_eq!(inc["match"]["any"], json!(["tg"]));
        // `exclude_sources` → must_not MatchAny on `source_name`.
        let must_not = v["must_not"].as_array().expect("must_not clauses");
        assert_eq!(must_not[0]["key"], "source_name");
        assert_eq!(must_not[0]["match"]["any"], json!(["claude-transcripts"]));
    }

    #[test]
    fn exclude_kinds_builds_must_not_match_any_on_kind() {
        use crate::aiplane::ipc::SearchFilter;
        let f = SearchFilter {
            exclude_kinds: vec!["claude-transcripts".into()],
            ..Default::default()
        };
        let v = build_filter(&f, None).expect("non-empty filter");
        let must_not = v["must_not"].as_array().expect("must_not clauses");
        let clause = must_not
            .iter()
            .find(|c| c["key"] == "kind")
            .expect("kind must_not clause");
        assert_eq!(clause["match"]["any"], json!(["claude-transcripts"]));
    }

    #[test]
    fn empty_filter_is_none() {
        use crate::aiplane::ipc::SearchFilter;
        assert!(build_filter(&SearchFilter::default(), None).is_none());
    }

    #[test]
    fn ensure_collection_requests_datetime_keyword_bool_indexes() {
        let want: Vec<(&str, &str)> = PAYLOAD_INDEXES.to_vec();
        assert!(want.contains(&("tags", "keyword")));
        assert!(want.contains(&("date", "datetime")));
        assert!(want.contains(&("kind", "keyword")));
        assert!(want.contains(&("source_name", "keyword")));
        assert!(want.contains(&("from", "keyword")));
        assert!(want.contains(&("has_media", "bool")));

        // Each entry compiles to a qdrant create-index request body.
        let body = index_create_body("date", "datetime");
        assert_eq!(body["field_name"], "date");
        assert_eq!(body["field_schema"], "datetime");
    }

    #[test]
    fn parse_version_reads_real_root_body() {
        // The shape qdrant's `GET /` returns (verified against 1.18.x).
        let body = r#"{"title":"qdrant - vector search engine","version":"1.18.1","commit":"abc"}"#;
        assert_eq!(parse_version(body), Some((1, 18)));
        // Two-component "major.minor" is enough; patch is ignored.
        assert_eq!(parse_version(r#"{"version":"1.16.0"}"#), Some((1, 16)));
        // Garbage / missing field → None (treated as "unknown" by callers).
        assert_eq!(parse_version("not json"), None);
        assert_eq!(parse_version(r#"{"version":"x.y"}"#), None);
        assert_eq!(parse_version(r#"{}"#), None);
    }

    #[test]
    fn meets_min_version_boundaries() {
        // RRF configurable `k` needs qdrant ≥ 1.16: 1.15 fails, 1.16 ok,
        // 1.18 ok (knowledge-retrieval-iter1 cross-cutting DoD).
        assert!(!meets_min_version((1, 15), MIN_HYBRID_VERSION));
        assert!(meets_min_version((1, 16), MIN_HYBRID_VERSION));
        assert!(meets_min_version((1, 18), MIN_HYBRID_VERSION));
        // A newer major always clears a 1.x minimum.
        assert!(meets_min_version((2, 0), MIN_HYBRID_VERSION));
        // An older major never clears it regardless of minor.
        assert!(!meets_min_version((0, 99), MIN_HYBRID_VERSION));
    }
}
