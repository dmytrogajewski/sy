//! Host-callable methods for the `sy file` plugin runtime (SPEC §4.2.5).
//!
//! Implements the seven host fns landing in roadmap Step 6 — the surface
//! journey beats **J3** (hover preview reads the source file) and **J6**
//! (copy progress emits a waybar pill) ride on:
//!
//! * `host.fs.read` — gated by `[needs].fs_read` glob list.
//! * `host.fs.cha` — same gate as `host.fs.read` (stat-shaped metadata).
//! * `host.fs.write_cache` — gated by `[needs].fs_write` containing
//!   `"cache"`; **atomic** write-temp-then-rename inside the
//!   per-plugin runtime slot.
//! * `host.notify.banner` — always allowed; emits onto the host-owned
//!   [`mpsc::Sender<Notification>`].
//! * `host.notify.waybar` — same gate as banner.
//! * `host.ui.theme` — always allowed; returns the current palette.
//! * `host.exec.run` — gated by `[needs].exec` containing `argv[0]`.
//!
//! Roadmap Step 27 added the previewer-side host fns:
//!
//! * `host.preview.image_show` — gated by `[needs].preview` containing
//!   `"image_show"`; the plugin hands the host a PNG payload to render
//!   in the file-manager preview pane. Decoded inside the handler and
//!   pushed onto the `HostCtx::preview_tx` channel the file plane
//!   owns.
//! * `host.preview.text` — same gate (`[needs].preview` containing
//!   `"text"`); plain UTF-8 body, pushed onto the same channel.
//!
//! The remaining deferred entries (`host.knowledge.*`,
//! `host.ui.confirm`) intentionally stay out of
//! [`crate::plugin::capability::HostCapabilities::ALL`] — their landing
//! roadmap steps re-enter that table and add their handlers here.
//!
//! [`dispatch`] is the single entry point the supervisor's reader loop
//! (Step 4 [`crate::plugin::proc`]) routes plugin-initiated requests
//! into. The `check_cap` decision reads from
//! [`HostCapabilities::knows`] so an unknown / future `host.*` method
//! returns a stable `METHOD_NOT_FOUND` error rather than silently
//! routing into a dead handler.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use globset::Glob;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::plugin::capability::HostCapabilities;
use crate::plugin::manifest::Manifest;
use crate::plugin::rpc::{CAP_NOT_GRANTED, INVALID_PATH};

/// JSON-RPC reserved code for "Method not found" (SPEC §4.2.2 inherits
/// the reserved range). Surfaces when a plugin invokes a `host.*`
/// method that is neither in [`HostCapabilities::ALL`] nor wired here.
/// Used by [`dispatch`] for unknown method names so the reader loop
/// never returns an opaque `Transport` error for a routable mistake.
pub const METHOD_NOT_FOUND: i32 = -32601;

/// JSON-RPC reserved code for "Invalid params" (SPEC §4.2.2 inherits
/// the reserved range). Surfaces when a known method receives params
/// that don't match its schema (e.g. `host.fs.read` without a `path`
/// field, `host.exec.run` without an `argv` array).
pub const INVALID_PARAMS: i32 = -32602;

/// Stable error returned by [`dispatch`]. Carries the same fields a
/// JSON-RPC 2.0 `error` object does so [`crate::plugin::proc`] can
/// serialise it back over the wire without further translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFnError {
    /// SPEC §4.2.2 numeric error code (e.g. `-32099 CAP_NOT_GRANTED`,
    /// `-32095 INVALID_PATH`, `-32601 METHOD_NOT_FOUND`).
    pub code: i32,
    /// Uppercase SPEC-style enum-name (`"CAP_NOT_GRANTED"`, etc.).
    pub message: String,
    /// Structured detail block for the plugin-side error handler.
    pub data: Value,
}

impl HostFnError {
    /// `-32099 CAP_NOT_GRANTED` with a `{ "needed": "<cap>" }` payload
    /// matching the SPEC §4.2.2 example shape.
    fn cap_not_granted(needed: &str) -> Self {
        Self {
            code: CAP_NOT_GRANTED,
            message: "CAP_NOT_GRANTED".into(),
            data: json!({ "needed": needed }),
        }
    }

    /// `-32095 INVALID_PATH` with the offending path echoed back.
    fn invalid_path(reason: &str, path: &str) -> Self {
        Self {
            code: INVALID_PATH,
            message: "INVALID_PATH".into(),
            data: json!({ "reason": reason, "path": path }),
        }
    }

    /// `-32601 METHOD_NOT_FOUND` for unknown `host.*` method names.
    fn method_not_found(method: &str) -> Self {
        Self {
            code: METHOD_NOT_FOUND,
            message: "METHOD_NOT_FOUND".into(),
            data: json!({ "method": method }),
        }
    }

    /// `-32602 INVALID_PARAMS` for known methods with bad params.
    fn invalid_params(reason: &str) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: "INVALID_PARAMS".into(),
            data: json!({ "reason": reason }),
        }
    }
}

/// Preview payload emitted by `host.preview.image_show` /
/// `host.preview.text`. The supervisor wires a
/// `mpsc::Sender<PreviewMessage>` into [`HostCtx`] so the file plane
/// (Step 27 `PluginBridge`) can subscribe; in tests + the CLI exec
/// path the channel is either absent (`preview_tx == None`) or the
/// receiver is owned by the test harness so we can assert payloads
/// land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewMessage {
    /// `host.preview.image_show` — decoded PNG bytes the plugin asked
    /// the host to render in the preview pane.
    Image {
        /// Plugin id (so a multi-plugin host can attribute the render).
        plugin_id: String,
        /// Decoded PNG body (base64 already stripped inside the
        /// handler).
        png_bytes: Vec<u8>,
    },
    /// `host.preview.text` — plain UTF-8 body the plugin asked the
    /// host to render in the preview pane.
    Text {
        /// Plugin id.
        plugin_id: String,
        /// Plain-text content.
        content: String,
    },
}

/// Notification emitted by `host.notify.banner` / `host.notify.waybar`.
/// The supervisor wires a `mpsc::Sender<Notification>` into the
/// [`HostCtx`] so the file-manager IPC layer (Step 20) can subscribe;
/// for Step 6 the receiver is owned by the test harness so we can
/// assert payloads land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// `host.notify.banner` — one-shot banner the host renders in the
    /// command bar.
    Banner {
        /// Plugin id (so a multi-plugin host can attribute the pill).
        plugin_id: String,
        /// Banner kind — e.g. `"info"`, `"warn"`, `"error"`.
        kind: String,
        /// Human-readable message body.
        message: String,
    },
    /// `host.notify.waybar` — waybar pill payload (the J6 "rendering…"
    /// indicator rides here).
    Waybar {
        /// Plugin id.
        plugin_id: String,
        /// Primary pill text.
        text: String,
        /// Optional hover tooltip.
        tooltip: String,
        /// Optional CSS class.
        class: String,
    },
}

/// Host-side context the supervisor passes into every [`dispatch`]
/// call. Owns the resources host fns need to interact with the rest of
/// the file manager / waybar / theme system.
///
/// Cheap to clone — every field is either `Arc`-wrapped or `Copy`.
#[derive(Debug, Clone)]
pub struct HostCtx {
    /// Per-plugin runtime slot — the cwd the sandbox layer pinned the
    /// child to. `host.fs.write_cache` lands under
    /// `<workdir>/cache/<name>`.
    pub workdir: Arc<PathBuf>,
    /// Notification channel — `host.notify.{banner,waybar}` push onto
    /// this; the file-manager IPC subscribes on the receive side.
    pub notify_tx: mpsc::Sender<Notification>,
    /// Theme palette returned by `host.ui.theme`. Stored as a JSON
    /// object so a future palette extension doesn't require a code
    /// change here.
    pub theme: Arc<Value>,
    /// Step 27 — channel `host.preview.image_show` / `host.preview.text`
    /// push onto so the file plane's preview pipeline picks the
    /// payload up. `None` for non-GUI / CLI-only contexts (e.g. `sy
    /// plugin exec`), in which case the host fns surface a stable
    /// `INVALID_PARAMS` body explaining the previewer channel is
    /// unwired rather than silently dropping the call.
    pub preview_tx: Option<mpsc::Sender<PreviewMessage>>,
}

/// Top-level dispatch for plugin-initiated host RPCs. The supervisor's
/// reader loop hands every plugin **request** (one carrying an `id`)
/// to this function; the returned `Result` is encoded back over the
/// same `FramedDuplex` as a JSON-RPC response.
///
/// Decision order:
///
/// 1. Reject methods outside [`HostCapabilities::ALL`] with
///    `METHOD_NOT_FOUND` (so a plugin built against a future host
///    can't trick this version into a half-implemented handler).
/// 2. Run the SPEC §4.2.5 `check_cap` row matching the method against
///    the plugin's manifest `[needs]` — fail with
///    `-32099 CAP_NOT_GRANTED` on miss.
/// 3. Route to the concrete handler.
#[tracing::instrument(skip(params, ctx, manifest), fields(plugin_id = %manifest.plugin.id))]
pub async fn dispatch(
    method: &str,
    params: Value,
    ctx: &HostCtx,
    manifest: &Manifest,
) -> Result<Value, HostFnError> {
    // SPEC §4.2.5 invariant: the host-callable surface is closed at
    // compile time by `HostCapabilities::ALL`. An unknown method here
    // is either a future host fn we don't yet implement, or a typo —
    // either way it must surface as a stable, peer-visible error
    // rather than slip into a default handler.
    if !HostCapabilities::knows(method) {
        return Err(HostFnError::method_not_found(method));
    }
    check_cap(method, &params, manifest)?;
    match method {
        // SPEC §4.2.5 row "host.fs.read".
        "host.fs.read" => host_fs_read(params, manifest).await,
        // SPEC §4.2.5 row "host.fs.cha".
        "host.fs.cha" => host_fs_cha(params, manifest).await,
        // SPEC §4.2.5 row "host.fs.write_cache".
        "host.fs.write_cache" => host_fs_write_cache(params, ctx).await,
        // SPEC §4.2.5 row "host.notify.banner".
        "host.notify.banner" => host_notify_banner(params, ctx, manifest).await,
        // SPEC §4.2.5 row "host.notify.waybar".
        "host.notify.waybar" => host_notify_waybar(params, ctx, manifest).await,
        // SPEC §4.2.5 row "host.ui.theme".
        "host.ui.theme" => host_ui_theme(ctx).await,
        // SPEC §4.2.5 row "host.exec.run".
        "host.exec.run" => host_exec_run(params, manifest).await,
        // SPEC §4.2.5 row "host.preview.image_show" (Step 27).
        "host.preview.image_show" => host_preview_image_show(params, ctx, manifest).await,
        // SPEC §4.2.5 row "host.preview.text" (Step 27).
        "host.preview.text" => host_preview_text(params, ctx, manifest).await,
        // `HostCapabilities::knows(...)` returned true above, so every
        // listed method must have a handler. If we ever drift, the
        // tests::dispatch_table_covers_every_host_capability check
        // surfaces the gap at build time.
        other => Err(HostFnError::method_not_found(other)),
    }
}

/// SPEC §4.3 `check_cap` — the capability ladder enforced at the host
/// RPC boundary. Reads the SPEC §4.2.5 "Required cap" column row by row
/// against the plugin's manifest `[needs]`.
fn check_cap(method: &str, params: &Value, manifest: &Manifest) -> Result<(), HostFnError> {
    match method {
        // SPEC §4.2.5: `host.fs.read` requires `fs_read` matches path.
        "host.fs.read" | "host.fs.cha" => check_fs_read_scope(params, manifest),
        // SPEC §4.2.5: `host.fs.write_cache` requires `fs_write`
        // contains `"cache"`.
        "host.fs.write_cache" => check_fs_write_cache_scope(manifest),
        // SPEC §4.2.5: `host.notify.*` and `host.ui.theme` rows say
        // "always allowed" — no `[needs]` check.
        "host.notify.banner" | "host.notify.waybar" | "host.ui.theme" => Ok(()),
        // SPEC §4.2.5: `host.exec.run` requires `exec` contains argv[0].
        "host.exec.run" => check_exec_argv(params, manifest),
        // SPEC §4.2.5: `host.preview.image_show` requires `[needs].preview`
        // contains `"image_show"` (Step 27).
        "host.preview.image_show" => check_preview_scope(manifest, "image_show"),
        // SPEC §4.2.5: `host.preview.text` requires `[needs].preview`
        // contains `"text"` (Step 27).
        "host.preview.text" => check_preview_scope(manifest, "text"),
        // `dispatch` guards us against unknown methods upstream.
        _ => Ok(()),
    }
}

/// `host.preview.*` capability ladder (Step 27). The manifest's
/// `[needs].preview` list must literally contain the sub-method
/// sentinel (`"image_show"` or `"text"`). Empty list → no preview
/// access — surfaces as `-32099 CAP_NOT_GRANTED`.
fn check_preview_scope(manifest: &Manifest, sentinel: &str) -> Result<(), HostFnError> {
    if !manifest.needs.preview.iter().any(|s| s == sentinel) {
        return Err(HostFnError::cap_not_granted("preview"));
    }
    Ok(())
}

/// `fs_read`/`fs_read`-shaped scope check. The manifest's
/// `[needs].fs_read` list is glob patterns; the path passed in the
/// request must match at least one of them. An empty list is the
/// SPEC's "no access" sentinel.
fn check_fs_read_scope(params: &Value, manifest: &Manifest) -> Result<(), HostFnError> {
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HostFnError::invalid_params("host.fs.* requires `path` string"))?;
    // Validate first; an out-of-set path is a CAP_NOT_GRANTED, but a
    // syntactically malformed path is INVALID_PATH (different SPEC
    // row, different error code).
    validate_path(path)?;
    if !path_matches_any(&manifest.needs.fs_read, path) {
        return Err(HostFnError::cap_not_granted("fs_read"));
    }
    Ok(())
}

/// `fs_write_cache` requires `[needs].fs_write` to literally contain
/// the `"cache"` sentinel (SPEC §4.2.5 row).
fn check_fs_write_cache_scope(manifest: &Manifest) -> Result<(), HostFnError> {
    if !manifest
        .needs
        .fs_write
        .iter()
        .any(|s| s.as_str() == "cache")
    {
        return Err(HostFnError::cap_not_granted("fs_write"));
    }
    Ok(())
}

/// `host.exec.run` requires the manifest's `[needs].exec` whitelist to
/// contain `argv[0]` (SPEC §4.2.5). Argv must be a non-empty string
/// array.
fn check_exec_argv(params: &Value, manifest: &Manifest) -> Result<(), HostFnError> {
    let argv0 = params
        .get("argv")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| HostFnError::invalid_params("host.exec.run requires non-empty `argv`"))?;
    if !manifest.needs.exec.iter().any(|s| s == argv0) {
        return Err(HostFnError::cap_not_granted("exec"));
    }
    Ok(())
}

/// Validate a path string at the host boundary: must be UTF-8 (already
/// is, since `serde_json` gave us a `&str`), must not contain a NUL
/// byte (kernels treat NUL as the path terminator), and must not be
/// empty. Anything malformed surfaces as `-32095 INVALID_PATH` per
/// SPEC §4.2.2.
fn validate_path(path: &str) -> Result<(), HostFnError> {
    if path.is_empty() {
        return Err(HostFnError::invalid_path("empty", path));
    }
    if path.contains('\0') {
        return Err(HostFnError::invalid_path("null-byte", path));
    }
    Ok(())
}

/// Return `true` if `path` matches at least one of the manifest globs
/// in `patterns`. Patterns compile via `globset::Glob`; a malformed
/// pattern is treated as a non-match (the manifest parser already
/// rejects bad globs at load time so this branch is defence-in-depth).
fn path_matches_any(patterns: &[String], path: &str) -> bool {
    for p in patterns {
        let Ok(g) = Glob::new(p) else { continue };
        if g.compile_matcher().is_match(path) {
            return true;
        }
    }
    false
}

/// `host.fs.read` — read `params.path` and return its body as
/// base64. The `max_bytes` field caps the read; absent → no cap.
async fn host_fs_read(params: Value, _manifest: &Manifest) -> Result<Value, HostFnError> {
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HostFnError::invalid_params("host.fs.read requires `path`"))?;
    let max_bytes = params.get("max_bytes").and_then(|v| v.as_u64());
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| HostFnError::invalid_path(&format!("read: {e}"), path))?;
    let body = match max_bytes {
        Some(cap) if (bytes.len() as u64) > cap => bytes[..(cap as usize)].to_vec(),
        _ => bytes,
    };
    Ok(json!({ "bytes_base64": base64_encode(&body) }))
}

/// `host.fs.cha` — stat-shaped metadata; returns `mtime`, `size`,
/// `mime` (best-effort via extension).
async fn host_fs_cha(params: Value, _manifest: &Manifest) -> Result<Value, HostFnError> {
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HostFnError::invalid_params("host.fs.cha requires `path`"))?;
    let md = tokio::fs::metadata(path)
        .await
        .map_err(|e| HostFnError::invalid_path(&format!("stat: {e}"), path))?;
    let mtime_ms = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mime = mime_for_extension(path);
    Ok(json!({ "mtime": mtime_ms, "size": md.len(), "mime": mime }))
}

/// `host.fs.write_cache` — atomic write under `<workdir>/cache/<name>`.
/// Implements the write-temp-then-rename pattern per the Step 6 brief:
/// the tmp file lives in the *same directory* as the final target so
/// the rename(2) is atomic on POSIX (same-filesystem requirement); on
/// rename failure the tmp is unlinked so no partial residue survives.
async fn host_fs_write_cache(params: Value, ctx: &HostCtx) -> Result<Value, HostFnError> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HostFnError::invalid_params("host.fs.write_cache requires `name`"))?;
    let b64 = params
        .get("bytes_base64")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            HostFnError::invalid_params("host.fs.write_cache requires `bytes_base64`")
        })?;
    // Reject path traversal — `name` must be a single relative
    // filename, never a path that escapes the cache slot.
    validate_cache_name(name)?;
    let bytes = base64_decode(b64).map_err(|_| {
        HostFnError::invalid_params("host.fs.write_cache `bytes_base64` is not valid base64")
    })?;
    let cache_dir = ctx.workdir.join("cache");
    tokio::fs::create_dir_all(&cache_dir).await.map_err(|e| {
        HostFnError::invalid_path(
            &format!("mkdir cache: {e}"),
            &cache_dir.display().to_string(),
        )
    })?;
    let final_path = cache_dir.join(name);
    let tmp_path = cache_dir.join(format!(".{name}.tmp"));
    write_atomic(&tmp_path, &final_path, &bytes).await?;
    Ok(json!({ "path": final_path.to_string_lossy() }))
}

/// Atomic write: write to `tmp`, fsync, rename to `final`. On rename
/// failure the tmp is unlinked so no half-written residue survives.
/// Same-directory rename is the POSIX rule that makes this atomic on
/// crash — `tmp` MUST live next to `final` on the same filesystem.
async fn write_atomic(tmp: &Path, final_path: &Path, bytes: &[u8]) -> Result<(), HostFnError> {
    let f = tokio::fs::File::create(tmp).await.map_err(|e| {
        HostFnError::invalid_path(&format!("create tmp: {e}"), &tmp.display().to_string())
    })?;
    // Scope: write + flush + sync, then drop the handle before rename
    // so the kernel's pagecache has the body persisted.
    {
        use tokio::io::AsyncWriteExt as _;
        let mut f = f;
        f.write_all(bytes).await.map_err(|e| {
            HostFnError::invalid_path(&format!("write tmp: {e}"), &tmp.display().to_string())
        })?;
        f.flush().await.map_err(|e| {
            HostFnError::invalid_path(&format!("flush tmp: {e}"), &tmp.display().to_string())
        })?;
        f.sync_all().await.map_err(|e| {
            HostFnError::invalid_path(&format!("fsync tmp: {e}"), &tmp.display().to_string())
        })?;
    }
    match tokio::fs::rename(tmp, final_path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort unlink so a failed atomic write doesn't leave
            // a partial file under the cache slot.
            let _ = tokio::fs::remove_file(tmp).await;
            Err(HostFnError::invalid_path(
                &format!("rename: {e}"),
                &final_path.display().to_string(),
            ))
        }
    }
}

/// `host.notify.banner` — emit a Banner onto the host channel.
async fn host_notify_banner(
    params: Value,
    ctx: &HostCtx,
    manifest: &Manifest,
) -> Result<Value, HostFnError> {
    let kind = params
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("info")
        .to_string();
    let message = params
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HostFnError::invalid_params("host.notify.banner requires `message`"))?
        .to_string();
    ctx.notify_tx
        .send(Notification::Banner {
            plugin_id: manifest.plugin.id.clone(),
            kind,
            message,
        })
        .await
        .map_err(|_| HostFnError::invalid_params("notify channel closed"))?;
    Ok(json!({ "ok": true }))
}

/// `host.notify.waybar` — emit a Waybar pill onto the host channel.
async fn host_notify_waybar(
    params: Value,
    ctx: &HostCtx,
    manifest: &Manifest,
) -> Result<Value, HostFnError> {
    let text = params
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HostFnError::invalid_params("host.notify.waybar requires `text`"))?
        .to_string();
    let tooltip = params
        .get("tooltip")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let class = params
        .get("class")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    ctx.notify_tx
        .send(Notification::Waybar {
            plugin_id: manifest.plugin.id.clone(),
            text,
            tooltip,
            class,
        })
        .await
        .map_err(|_| HostFnError::invalid_params("notify channel closed"))?;
    Ok(json!({ "ok": true }))
}

/// `host.ui.theme` — return the host's current theme palette. Stored
/// on the `HostCtx` so a future palette-swap doesn't propagate through
/// every plugin process; plugins re-call this to refresh.
async fn host_ui_theme(ctx: &HostCtx) -> Result<Value, HostFnError> {
    Ok(json!({ "palette": (*ctx.theme).clone() }))
}

/// `host.exec.run` — spawn a subprocess from the manifest's
/// `[needs].exec` allowlist. Returns the captured exit status + stdout
/// + stderr base64-encoded so the body crosses the JSON wire intact.
async fn host_exec_run(params: Value, _manifest: &Manifest) -> Result<Value, HostFnError> {
    let argv: Vec<String> = params
        .get("argv")
        .and_then(|v| v.as_array())
        .ok_or_else(|| HostFnError::invalid_params("host.exec.run requires `argv` array"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| HostFnError::invalid_params("host.exec.run argv is empty"))?;
    let stdin_b64 = params.get("stdin").and_then(|v| v.as_str());
    let stdin_bytes = match stdin_b64 {
        None => Vec::new(),
        Some(s) => base64_decode(s)
            .map_err(|_| HostFnError::invalid_params("host.exec.run stdin not base64"))?,
    };
    let out = run_subprocess(program, args, &stdin_bytes).await?;
    Ok(json!({
        "status": out.status,
        "stdout_base64": base64_encode(&out.stdout),
        "stderr_base64": base64_encode(&out.stderr),
    }))
}

/// `host.preview.image_show` (Step 27) — decode the plugin's PNG
/// payload at the host boundary (the host owns the base64 crate-equivalent
/// alphabet table) and push it onto the file plane's preview channel.
///
/// Returns `INVALID_PARAMS` when `preview_tx` is `None` (the calling
/// context has no UI to receive the payload — e.g. `sy plugin exec`).
/// This is a stable error rather than `Ok({})` because the plugin
/// asked the host to *render something* and the host did not, so the
/// plugin needs to know.
async fn host_preview_image_show(
    params: Value,
    ctx: &HostCtx,
    manifest: &Manifest,
) -> Result<Value, HostFnError> {
    let b64 = params
        .get("png_base64")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            HostFnError::invalid_params("host.preview.image_show requires `png_base64`")
        })?;
    let png_bytes = base64_decode(b64).map_err(|_| {
        HostFnError::invalid_params("host.preview.image_show `png_base64` is not valid base64")
    })?;
    let Some(tx) = ctx.preview_tx.as_ref() else {
        return Err(HostFnError::invalid_params(
            "host.preview.image_show: preview channel unwired (no preview pane in this context)",
        ));
    };
    tx.send(PreviewMessage::Image {
        plugin_id: manifest.plugin.id.clone(),
        png_bytes,
    })
    .await
    .map_err(|_| HostFnError::invalid_params("preview channel closed"))?;
    Ok(json!({ "ok": true }))
}

/// `host.preview.text` (Step 27) — push a plain UTF-8 body onto the
/// file plane's preview channel. Mirror-image of
/// [`host_preview_image_show`]; the body is forwarded verbatim so the
/// file plane's renderer (syntect / cosmic-text) decides how to lay it
/// out.
async fn host_preview_text(
    params: Value,
    ctx: &HostCtx,
    manifest: &Manifest,
) -> Result<Value, HostFnError> {
    let content = params
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HostFnError::invalid_params("host.preview.text requires `content`"))?
        .to_string();
    let Some(tx) = ctx.preview_tx.as_ref() else {
        return Err(HostFnError::invalid_params(
            "host.preview.text: preview channel unwired (no preview pane in this context)",
        ));
    };
    tx.send(PreviewMessage::Text {
        plugin_id: manifest.plugin.id.clone(),
        content,
    })
    .await
    .map_err(|_| HostFnError::invalid_params("preview channel closed"))?;
    Ok(json!({ "ok": true }))
}

/// Captured output of a host-side subprocess. Plain struct so the
/// `run_subprocess` helper has a single return type the
/// `host.exec.run` handler can serialise.
struct CapturedOutput {
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Spawn `program` with `args`, feed `stdin_bytes` (may be empty),
/// capture stdout + stderr. Plain `tokio::process::Command` —
/// `host.exec.run`'s manifest-side allowlist already gated the call.
async fn run_subprocess(
    program: &str,
    args: &[String],
    stdin_bytes: &[u8],
) -> Result<CapturedOutput, HostFnError> {
    use tokio::io::AsyncWriteExt as _;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| HostFnError::invalid_params(&format!("host.exec.run spawn: {e}")))?;
    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(stdin_bytes).await;
        let _ = sin.shutdown().await;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| HostFnError::invalid_params(&format!("host.exec.run wait: {e}")))?;
    Ok(CapturedOutput {
        status: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// Reject any `name` that contains a path separator or `..`. The cache
/// slot is the per-plugin runtime dir; a `name` like `../etc/passwd`
/// would otherwise escape the sandbox.
fn validate_cache_name(name: &str) -> Result<(), HostFnError> {
    if name.is_empty() {
        return Err(HostFnError::invalid_params("cache name is empty"));
    }
    let p = Path::new(name);
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            _ => {
                return Err(HostFnError::invalid_path(
                    "cache name must be a single relative file",
                    name,
                ))
            }
        }
    }
    Ok(())
}

/// Best-effort MIME via file extension. `tree_magic_mini` is the SPEC
/// §3.3 item 19 sniff path (lands in Step 19); until then a small
/// table covers the previewer canary's `.md` / `.txt` / `.png` cases.
fn mime_for_extension(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".md") || lower.ends_with(".markdown") {
        "text/markdown"
    } else if lower.ends_with(".txt") {
        "text/plain"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else {
        "application/octet-stream"
    }
}

/// Canonical base64 alphabet (RFC 4648 §4). Inlined so we don't pull a
/// `base64` direct dep just for the two encode/decode call sites the
/// host fns need (encoding the read body + decoding the write_cache
/// input). The two transitive `base64` versions in `Cargo.lock` come
/// in via build-time deps we don't expose to the bin.
const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `input` as base64. Trivial loop over 24-bit triples; pad
/// with `=` on the trailing partial triple.
fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let chunks = input.chunks(3);
    for c in chunks {
        let (b0, b1, b2) = match c.len() {
            3 => (c[0], c[1], c[2]),
            2 => (c[0], c[1], 0),
            1 => (c[0], 0, 0),
            _ => unreachable!("chunks(3) cannot yield 0"),
        };
        let n: u32 = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        match c.len() {
            3 => {
                out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
                out.push(B64_ALPHABET[(n & 0x3f) as usize] as char);
            }
            2 => {
                out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
                out.push('=');
            }
            1 => {
                out.push('=');
                out.push('=');
            }
            _ => unreachable!(),
        }
    }
    out
}

/// Build the inverse alphabet for decode. Returns 255 for "invalid".
fn b64_decode_table() -> [u8; 256] {
    let mut t = [255u8; 256];
    for (i, b) in B64_ALPHABET.iter().enumerate() {
        t[*b as usize] = i as u8;
    }
    t
}

/// Decode an RFC 4648 base64 string. Accepts canonical input with
/// padding; rejects any non-alphabet character outside trailing `=`.
fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(());
    }
    let table = b64_decode_table();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut chunk = [0u8; 4];
    let mut pad = 0usize;
    for c in bytes.chunks(4) {
        pad = 0;
        for (i, b) in c.iter().enumerate() {
            if *b == b'=' {
                chunk[i] = 0;
                pad += 1;
            } else {
                let v = table[*b as usize];
                if v == 255 {
                    return Err(());
                }
                chunk[i] = v;
            }
        }
        let n: u32 = (u32::from(chunk[0]) << 18)
            | (u32::from(chunk[1]) << 12)
            | (u32::from(chunk[2]) << 6)
            | u32::from(chunk[3]);
        out.push(((n >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((n & 0xff) as u8);
        }
    }
    // pad > 0 may only appear on the last chunk; the loop overwrites
    // `pad` every iteration so the trailing value is what matters.
    let _ = pad;
    Ok(out)
}

/// Convenience constructor for unit tests + the supervisor's setup
/// path: builds a [`HostCtx`] with the given workdir, an mpsc channel
/// (caller keeps the receiver), and a default-empty theme.
///
/// `preview_tx` is left `None` so legacy callers (Step 6 unit tests,
/// `sy plugin exec`'s CLI lane — no preview pane to receive a render
/// in either case) keep their current semantics. Step 27's
/// `PluginBridge` uses [`ctx_for_with_preview`] instead so the
/// `host.preview.*` host fns reach the file plane's UI thread.
pub fn ctx_for(workdir: PathBuf, theme: Value) -> (HostCtx, mpsc::Receiver<Notification>) {
    let (tx, rx) = mpsc::channel(32);
    (
        HostCtx {
            workdir: Arc::new(workdir),
            notify_tx: tx,
            theme: Arc::new(theme),
            preview_tx: None,
        },
        rx,
    )
}

/// Step 27 convenience constructor: builds a [`HostCtx`] with both
/// channels wired (notify + preview). Returns the two receivers so the
/// caller can subscribe to both surfaces. Used by the file plane's
/// `PluginBridge` and by `tests/sy_file_plugin_preview.rs` to assert
/// `host.preview.image_show` / `host.preview.text` round-trip
/// correctly.
pub fn ctx_for_with_preview(
    workdir: PathBuf,
    theme: Value,
) -> (
    HostCtx,
    mpsc::Receiver<Notification>,
    mpsc::Receiver<PreviewMessage>,
) {
    let (notify_tx, notify_rx) = mpsc::channel(32);
    let (preview_tx, preview_rx) = mpsc::channel(32);
    (
        HostCtx {
            workdir: Arc::new(workdir),
            notify_tx,
            theme: Arc::new(theme),
            preview_tx: Some(preview_tx),
        },
        notify_rx,
        preview_rx,
    )
}

/// Convenience for builder-style theme construction in tests. Returns
/// an empty `BTreeMap` of palette entries serialised to a JSON object.
#[cfg(test)]
pub(crate) fn empty_theme() -> Value {
    serde_json::to_value(std::collections::BTreeMap::<String, String>::new()).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    //! Unit tests for the seven Step 6 host fns. Every behaviour the
    //! roadmap brief calls out gets one named test asserting the
    //! public surface (`dispatch` return value + side effects). The
    //! E2E `step06_…` test in `tests/sy_file_journey_e2e.rs` drives
    //! the same surface through the real supervisor wire path.
    use super::*;
    use crate::plugin::manifest::load;

    /// Minimal manifest fixture. Callers override the `[needs]` block
    /// via [`with_needs`] to cover the various capability rows.
    const BASE_MANIFEST: &str = r#"
api = "1"

[plugin]
id = "sy-plugin-hostfn-test"
name = "Host Fn Test"
version = "0.0.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "/bin/true"

[[capability]]
kind = "previewer"
mime = "text/markdown"

[needs]
fs_read = []
fs_write = []
preview = []
knowledge = []
network = []
exec = []

[limits]
memory_mb = 64
cpu_seconds = 10
nofile = 64
spawn_timeout_ms = 250
shutdown_timeout_ms = 500

[env]
PATH = "/usr/bin:/bin"
"#;

    /// Patch the `[needs]` block with the named arrays. Each `(field,
    /// toml_value)` replaces the line `<field> = []` with `<field> =
    /// <toml_value>` so the test can express realistic scopes (e.g.
    /// `fs_read = ["**/*.md"]`).
    fn with_needs(patches: &[(&str, &str)]) -> Manifest {
        let mut src = BASE_MANIFEST.to_string();
        for (field, value) in patches {
            let needle = format!("{field} = []");
            let replacement = format!("{field} = {value}");
            src = src.replace(&needle, &replacement);
        }
        load(&src).expect("manifest patches parse + validate")
    }

    /// SPEC §4.2.5 row "host.fs.read" + Step 6 brief:
    /// manifest `fs_read = ["**/*.md"]`; `path = "fixtures/sample.md"`
    /// returns the file's body as base64. Asserts the gate matches
    /// in-scope and the body round-trips through the base64 encoder.
    #[tokio::test]
    async fn fs_read_in_scope_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        let sample = tmp.path().join("sample.md");
        std::fs::write(&sample, b"# hello\n").expect("write sample");
        let manifest = with_needs(&[("fs_read", r#"["**/*.md"]"#)]);
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        let v = dispatch(
            "host.fs.read",
            json!({ "path": sample.to_string_lossy() }),
            &ctx,
            &manifest,
        )
        .await
        .expect("in-scope read succeeds");
        let b64 = v["bytes_base64"].as_str().expect("bytes_base64 present");
        let decoded = base64_decode(b64).expect("base64 decodes");
        assert_eq!(decoded, b"# hello\n");
    }

    /// SPEC §4.2.5 row "host.fs.read" gate: manifest scopes to
    /// `["**/*.md"]` so `/etc/passwd` is out of scope and the host
    /// must surface `-32099 CAP_NOT_GRANTED`.
    #[tokio::test]
    async fn fs_read_out_of_scope_returns_cap_not_granted() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[("fs_read", r#"["**/*.md"]"#)]);
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        let err = dispatch(
            "host.fs.read",
            json!({ "path": "/etc/passwd" }),
            &ctx,
            &manifest,
        )
        .await
        .expect_err("out-of-scope read must fail");
        assert_eq!(err.code, CAP_NOT_GRANTED, "must surface -32099");
        assert_eq!(err.message, "CAP_NOT_GRANTED");
        assert_eq!(err.data["needed"], "fs_read");
    }

    /// SPEC §4.2.5 row "host.fs.write_cache" + atomicity requirement:
    /// write lands under `<workdir>/cache/<name>` and the partial file
    /// never appears at the final name (rename-once contract).
    #[tokio::test]
    async fn fs_write_cache_lands_in_xdg_runtime_subdir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[("fs_write", r#"["cache"]"#)]);
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        let body = b"persisted-by-write_cache".to_vec();
        let resp = dispatch(
            "host.fs.write_cache",
            json!({ "name": "preview.bin", "bytes_base64": base64_encode(&body) }),
            &ctx,
            &manifest,
        )
        .await
        .expect("write_cache must succeed");
        let final_path = resp["path"].as_str().expect("path present");
        let written = std::fs::read(final_path).expect("final file readable");
        assert_eq!(written, body, "final body must match input");
        let want_dir = tmp.path().join("cache");
        let final_buf = std::path::PathBuf::from(final_path);
        assert!(
            final_buf.starts_with(&want_dir),
            "path must live under <workdir>/cache, got {final_path:?}"
        );
        // Atomicity: no tmp file remains after a clean write.
        let tmp_residue = want_dir.join(".preview.bin.tmp");
        assert!(
            !tmp_residue.exists(),
            ".tmp residue must be gone post-rename, found at {tmp_residue:?}"
        );
    }

    /// SPEC §4.2.5 row "host.notify.waybar": dispatch emits a
    /// `Waybar` payload onto the host-owned `mpsc::Sender`, which the
    /// receiver end (held by the file-manager IPC in production /
    /// test harness here) observes.
    #[tokio::test]
    async fn notify_waybar_round_trips_to_ipc() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[]);
        let (ctx, mut rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        let resp = dispatch(
            "host.notify.waybar",
            json!({ "text": "rendering…", "tooltip": "preview building", "class": "info" }),
            &ctx,
            &manifest,
        )
        .await
        .expect("notify.waybar succeeds");
        assert_eq!(resp["ok"], true);
        let got = rx.recv().await.expect("receiver sees the notification");
        match got {
            Notification::Waybar {
                plugin_id,
                text,
                tooltip,
                class,
            } => {
                assert_eq!(plugin_id, "sy-plugin-hostfn-test");
                assert_eq!(text, "rendering…");
                assert_eq!(tooltip, "preview building");
                assert_eq!(class, "info");
            }
            other => panic!("expected Waybar, got {other:?}"),
        }
    }

    /// SPEC §4.2.5 row "host.exec.run": argv[0] must appear in
    /// manifest `[needs].exec`. Allowed binary (`pdftoppm` in the
    /// SPEC example; here we substitute `/bin/echo` because the test
    /// host always has it) succeeds; an unlisted `rm` returns
    /// `-32099 CAP_NOT_GRANTED`.
    #[tokio::test]
    async fn host_exec_run_whitelist() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[("exec", r#"["/bin/echo"]"#)]);
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        // Allowed: /bin/echo with one arg.
        let ok = dispatch(
            "host.exec.run",
            json!({ "argv": ["/bin/echo", "hello"] }),
            &ctx,
            &manifest,
        )
        .await
        .expect("whitelisted exec succeeds");
        assert_eq!(ok["status"], 0);
        let stdout = base64_decode(ok["stdout_base64"].as_str().expect("stdout present"))
            .expect("base64 decodes");
        assert_eq!(stdout, b"hello\n");

        // Rejected: rm not in the whitelist.
        let err = dispatch(
            "host.exec.run",
            json!({ "argv": ["rm", "-rf", "/"] }),
            &ctx,
            &manifest,
        )
        .await
        .expect_err("non-whitelisted exec must fail");
        assert_eq!(err.code, CAP_NOT_GRANTED);
        assert_eq!(err.data["needed"], "exec");
    }

    /// SPEC §4.2.2 row "INVALID_PATH" — a NUL-byte in the path must
    /// surface as `-32095 INVALID_PATH`, not silently fall through to
    /// a kernel ENOENT. The host validates at the boundary so plugins
    /// can't probe filesystem state via timing of error variants.
    #[tokio::test]
    async fn invalid_path_returns_32095() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[("fs_read", r#"["**/*"]"#)]);
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        let err = dispatch(
            "host.fs.read",
            json!({ "path": "/etc/passwd\0null" }),
            &ctx,
            &manifest,
        )
        .await
        .expect_err("null-byte path must fail");
        assert_eq!(err.code, INVALID_PATH, "must surface -32095");
        assert_eq!(err.message, "INVALID_PATH");
        assert_eq!(err.data["reason"], "null-byte");
    }

    /// SPEC §4.2.5 source-of-truth invariant: every method listed in
    /// `HostCapabilities::ALL` must have a `dispatch` handler. The
    /// Step 6 brief makes this a build-time guard — if a future
    /// commit adds an entry but forgets the handler, the test
    /// surfaces the gap.
    #[tokio::test]
    async fn dispatch_table_covers_every_host_capability() {
        let tmp = tempfile::tempdir().expect("tmp");
        // Manifest with every gate satisfied so the only failure
        // mode this test exercises is the "method has no handler"
        // path — i.e. METHOD_NOT_FOUND from dispatch.
        let manifest = with_needs(&[
            ("fs_read", r#"["**/*"]"#),
            ("fs_write", r#"["cache"]"#),
            ("exec", r#"["/bin/true"]"#),
        ]);
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        for name in HostCapabilities::method_names() {
            // We don't actually care about the result — only that
            // dispatch doesn't return METHOD_NOT_FOUND, which would
            // mean the entry is in the table but unrouted.
            let res = dispatch(name, json!({}), &ctx, &manifest).await;
            if let Err(e) = &res {
                assert_ne!(
                    e.code, METHOD_NOT_FOUND,
                    "HostCapabilities::ALL row {name} has no handler in dispatch"
                );
            }
        }
    }

    /// Unknown method names get METHOD_NOT_FOUND, not a silent route
    /// into a default handler. Defence-in-depth so a future plugin
    /// calling a still-deferred host fn (`host.knowledge.query`,
    /// `host.ui.confirm`) gets a clear error instead of a hang. The
    /// preview namespace landed in Step 27 — see
    /// [`preview_image_show_round_trips_to_channel`] for the routed
    /// path.
    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[]);
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        let err = dispatch("host.knowledge.query", json!({}), &ctx, &manifest)
            .await
            .expect_err("deferred host fn must surface as METHOD_NOT_FOUND today");
        assert_eq!(err.code, METHOD_NOT_FOUND);
    }

    /// SPEC §4.2.5 row "host.preview.image_show" (Step 27): a manifest
    /// granting `preview = ["image_show"]` and a wired preview channel
    /// round-trip a PNG payload to the file plane's receiver.
    #[tokio::test]
    async fn preview_image_show_round_trips_to_channel() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[("preview", r#"["image_show"]"#)]);
        let (ctx, _notify_rx, mut preview_rx) =
            ctx_for_with_preview(tmp.path().to_path_buf(), empty_theme());
        let body = b"\x89PNG\r\n\x1a\nSTUB".to_vec();
        let resp = dispatch(
            "host.preview.image_show",
            json!({ "png_base64": base64_encode(&body) }),
            &ctx,
            &manifest,
        )
        .await
        .expect("image_show with preview cap + channel must succeed");
        assert_eq!(resp["ok"], true);
        let msg = preview_rx.recv().await.expect("receiver sees the payload");
        match msg {
            PreviewMessage::Image {
                plugin_id,
                png_bytes,
            } => {
                assert_eq!(plugin_id, "sy-plugin-hostfn-test");
                assert_eq!(png_bytes, body);
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    /// SPEC §4.2.5 row "host.preview.image_show" gate (Step 27): the
    /// manifest's `[needs].preview` list must literally contain
    /// `"image_show"`. An empty preview list surfaces as
    /// `-32099 CAP_NOT_GRANTED`.
    #[tokio::test]
    async fn preview_image_show_without_cap_returns_cap_not_granted() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[]);
        let (ctx, _notify_rx, _preview_rx) =
            ctx_for_with_preview(tmp.path().to_path_buf(), empty_theme());
        let err = dispatch(
            "host.preview.image_show",
            json!({ "png_base64": "" }),
            &ctx,
            &manifest,
        )
        .await
        .expect_err("image_show without cap must fail");
        assert_eq!(err.code, CAP_NOT_GRANTED);
        assert_eq!(err.data["needed"], "preview");
    }

    /// SPEC §4.2.5 row "host.preview.text" (Step 27): a manifest
    /// granting `preview = ["text"]` and a wired channel round-trip a
    /// UTF-8 body to the file plane's receiver.
    #[tokio::test]
    async fn preview_text_round_trips_to_channel() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[("preview", r#"["text"]"#)]);
        let (ctx, _notify_rx, mut preview_rx) =
            ctx_for_with_preview(tmp.path().to_path_buf(), empty_theme());
        let resp = dispatch(
            "host.preview.text",
            json!({ "content": "hello plugin" }),
            &ctx,
            &manifest,
        )
        .await
        .expect("preview.text with cap + channel must succeed");
        assert_eq!(resp["ok"], true);
        let msg = preview_rx.recv().await.expect("receiver sees the payload");
        match msg {
            PreviewMessage::Text { plugin_id, content } => {
                assert_eq!(plugin_id, "sy-plugin-hostfn-test");
                assert_eq!(content, "hello plugin");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    /// `host.fs.cha` returns a stat-shaped object — mtime + size +
    /// mime — gated by the same `fs_read` scope as `host.fs.read`.
    #[tokio::test]
    async fn fs_cha_returns_stat_shape() {
        let tmp = tempfile::tempdir().expect("tmp");
        let sample = tmp.path().join("sample.md");
        std::fs::write(&sample, b"# hello\n").expect("write sample");
        let manifest = with_needs(&[("fs_read", r#"["**/*.md"]"#)]);
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        let v = dispatch(
            "host.fs.cha",
            json!({ "path": sample.to_string_lossy() }),
            &ctx,
            &manifest,
        )
        .await
        .expect("host.fs.cha succeeds in scope");
        assert_eq!(v["size"], 8);
        assert_eq!(v["mime"], "text/markdown");
        assert!(v["mtime"].as_u64().unwrap_or(0) > 0);
    }

    /// `host.ui.theme` returns whatever palette the host pinned at
    /// startup — used by previewers so PNGs match the file-manager
    /// chrome (J3 hover renders to the active theme).
    #[tokio::test]
    async fn ui_theme_returns_palette() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[]);
        let theme = json!({ "bg": "#1d2021", "fg": "#ebdbb2", "accent": "#d65d0e" });
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), theme.clone());
        let v = dispatch("host.ui.theme", json!(null), &ctx, &manifest)
            .await
            .expect("theme always allowed");
        assert_eq!(v["palette"], theme);
    }

    /// Base64 round-trips for the encoder/decoder. Covers the three
    /// padding lengths (0/1/2 bytes of trailing data).
    #[test]
    fn base64_round_trip() {
        for body in [
            &b""[..],
            &b"f"[..],
            &b"fo"[..],
            &b"foo"[..],
            &b"foob"[..],
            &b"fooba"[..],
            &b"foobar"[..],
        ] {
            let enc = base64_encode(body);
            let dec = base64_decode(&enc).expect("round trip decodes");
            assert_eq!(dec, body, "body {body:?} must round-trip");
        }
    }

    /// Cache name must not escape the cache slot. The brief lists path
    /// traversal as a Step 6 hardening concern; defence-in-depth.
    #[tokio::test]
    async fn write_cache_rejects_path_traversal() {
        let tmp = tempfile::tempdir().expect("tmp");
        let manifest = with_needs(&[("fs_write", r#"["cache"]"#)]);
        let (ctx, _rx) = ctx_for(tmp.path().to_path_buf(), empty_theme());
        let err = dispatch(
            "host.fs.write_cache",
            json!({ "name": "../escape.txt", "bytes_base64": base64_encode(b"x") }),
            &ctx,
            &manifest,
        )
        .await
        .expect_err("path traversal must be rejected");
        assert_eq!(err.code, INVALID_PATH);
    }
}
