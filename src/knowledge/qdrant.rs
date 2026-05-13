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

use super::{exit, COLLECTION, QDRANT_PORT, VECTOR_DIM};

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
        let body = json!({
            "vectors": {
                "size": VECTOR_DIM,
                "distance": "Cosine"
            }
        });
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
    // Payload indexes — best-effort. Re-creating an existing index is a
    // 200 in qdrant 1.x, so this is safe to call on every startup.
    let _ = ensure_payload_index("tags", "keyword");
    Ok(())
}

/// Idempotently create a payload index on `field_name`. `field_schema`
/// is the qdrant payload schema string (`"keyword"`, `"integer"`, …).
pub fn ensure_payload_index(field_name: &str, field_schema: &str) -> Result<()> {
    let c = client()?;
    let url = format!("{}/collections/{}/index", base_url(), COLLECTION);
    let body = json!({
        "field_name": field_name,
        "field_schema": field_schema,
    });
    let resp = c.put(&url).json(&body).send().context("create index")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().unwrap_or_default();
        anyhow::bail!("qdrant: create index {field_name} failed ({status}): {txt}");
    }
    Ok(())
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
    pub payload: PointPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Upsert a batch of points. Caller is responsible for batching.
pub fn upsert(points: &[Point]) -> Result<()> {
    if points.is_empty() {
        return Ok(());
    }
    let c = client()?;
    let url = format!("{}/collections/{}/points?wait=true", base_url(), COLLECTION);
    let body = json!({
        "points": points.iter().map(|p| json!({
            "id": p.id,
            "vector": p.vector,
            "payload": p.payload,
        })).collect::<Vec<_>>()
    });
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

/// Top-k cosine search. `prefix_filter` restricts hits to file paths
/// starting with the given string.
pub fn search(vector: &[f32], limit: usize, prefix_filter: Option<&str>) -> Result<Vec<SearchHit>> {
    let c = client()?;
    let url = format!("{}/collections/{}/points/search", base_url(), COLLECTION);
    let mut body = json!({
        "vector": vector,
        "limit": limit,
        "with_payload": true,
    });
    if let Some(prefix) = prefix_filter {
        body["filter"] = json!({
            "must": [{
                "key": "file_path",
                "match": { "text": prefix }
            }]
        });
    }
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
