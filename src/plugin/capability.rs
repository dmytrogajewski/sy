//! Capability negotiation for the `sy file` plugin runtime.
//!
//! Implements the host side of the SPEC §4.2.3 lifecycle handshake —
//! the [`initialize`][initialize-row] request/response pair the
//! supervisor (Step 4) drives the moment the child child stdio is
//! framed. The host advertises:
//!
//! * the supported `api` array, and
//! * the set of host-callable method names the plugin may invoke from
//!   the SPEC §4.2.5 namespace,
//!
//! and the plugin's response carries back:
//!
//! * the `api` value it selected (must be ∈ host's advertised set —
//!   else [`API_VERSION_MISMATCH`]),
//! * its `[[capability]]` set (must be ⊆ manifest's declared
//!   `[[capability]]` rows — else [`RpcError::Protocol`]), and
//! * the host methods it `offers` to call (unknown names get a
//!   `tracing::warn!` and are dropped — forward-compat per SPEC §4.1).
//!
//! [`HostCapabilities::ALL`] is the **single source of truth** for the
//! host-callable namespace: the `initialize` payload constructor
//! reads from it and the Step 6 runtime cap-check enforcer
//! (`host_fns::dispatch`) will too. Adding a new `host.*` method is a
//! single-edit in this table.
//!
//! [initialize-row]: ../../../specs/research/sy-file-manager-plugins/SPEC.md#423-lifecycle-methods-host-→-plugin
use crate::plugin::manifest::{Capability, Manifest};
use crate::plugin::proc::RpcError;
use crate::plugin::rpc::API_VERSION_MISMATCH;

/// SPEC §4.2.5 host-callable methods the host plane exposes to plugins.
///
/// Each variant maps 1:1 to a row in the SPEC §4.2.5 table. The
/// [`HostCapability::method_name`] returns the dotted JSON-RPC method
/// string the plugin uses when invoking the host fn. The set is
/// closed at compile time — adding a new method is a single-line edit
/// in [`HostCapabilities::ALL`].
///
/// **Scope note (roadmap Step 27):** Step 6 landed the first seven
/// host fns; Step 27 added `host.preview.image_show` +
/// `host.preview.text` as part of the J3 plugin-routed preview
/// pipeline. The remaining two (`host.knowledge.query`,
/// `host.ui.confirm`) stay deferred to later roadmap steps and are
/// intentionally excluded from [`HostCapabilities::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostCapability {
    /// SPEC §4.2.5 `host.fs.read` — read bytes from a path scoped by
    /// `[needs].fs_read`.
    FsRead,
    /// SPEC §4.2.5 `host.fs.cha` — stat-shaped metadata (mtime / size
    /// / mime), scoped by `[needs].fs_read`.
    FsCha,
    /// SPEC §4.2.5 `host.fs.write_cache` — write bytes into the
    /// plugin's cache slot, scoped by `[needs].fs_write`.
    FsWriteCache,
    /// SPEC §4.2.5 `host.notify.waybar` — push a waybar pill (always
    /// allowed; J6's "rendering…" indicator rides here).
    NotifyWaybar,
    /// SPEC §4.2.5 `host.notify.banner` — push a one-shot banner
    /// (always allowed).
    NotifyBanner,
    /// SPEC §4.2.5 `host.ui.theme` — read the active palette (always
    /// allowed; previewers use this so PNGs match the file-manager
    /// chrome).
    UiTheme,
    /// SPEC §4.2.5 `host.exec.run` — spawn a subprocess from the
    /// `[needs].exec` allowlist.
    ExecRun,
    /// SPEC §4.2.5 `host.preview.image_show` — plugin hands the host a
    /// PNG payload to render in the preview pane (J3 plugin-routed
    /// dispatch, roadmap Step 27). Gated by `[needs].preview`
    /// containing `"image_show"`.
    PreviewImageShow,
    /// SPEC §4.2.5 `host.preview.text` — plugin hands the host a text
    /// body to render in the preview pane. Gated by `[needs].preview`
    /// containing `"text"`.
    PreviewText,
}

impl HostCapability {
    /// Dotted JSON-RPC method name (the wire string the plugin emits
    /// when calling this host fn).
    pub const fn method_name(self) -> &'static str {
        match self {
            HostCapability::FsRead => "host.fs.read",
            HostCapability::FsCha => "host.fs.cha",
            HostCapability::FsWriteCache => "host.fs.write_cache",
            HostCapability::NotifyWaybar => "host.notify.waybar",
            HostCapability::NotifyBanner => "host.notify.banner",
            HostCapability::UiTheme => "host.ui.theme",
            HostCapability::ExecRun => "host.exec.run",
            HostCapability::PreviewImageShow => "host.preview.image_show",
            HostCapability::PreviewText => "host.preview.text",
        }
    }
}

/// Compile-time table of every host-callable method the host plane
/// currently implements. The `initialize` payload constructor and the
/// Step 6 runtime cap-check enforcer both read from this constant —
/// it is the canonical single source of truth for the host-callable
/// surface.
pub struct HostCapabilities;

impl HostCapabilities {
    /// The host-callable methods landing in roadmap Steps 6 + 27. The
    /// remaining deferred entries (`host.knowledge.*`,
    /// `host.ui.confirm`) re-enter this table in their landing step.
    pub const ALL: &'static [HostCapability] = &[
        HostCapability::FsRead,
        HostCapability::FsCha,
        HostCapability::FsWriteCache,
        HostCapability::NotifyWaybar,
        HostCapability::NotifyBanner,
        HostCapability::UiTheme,
        HostCapability::ExecRun,
        HostCapability::PreviewImageShow,
        HostCapability::PreviewText,
    ];

    /// Iterator of the wire method names from [`Self::ALL`] — the
    /// `initialize.params.host.host_methods` array the host sends to
    /// the plugin so plugins know which `host.*` callbacks are
    /// reachable in this host version.
    pub fn method_names() -> impl Iterator<Item = &'static str> {
        Self::ALL.iter().map(|c| c.method_name())
    }

    /// Returns `true` if `name` is the wire method name of a host
    /// capability in [`Self::ALL`]. The Step 6 dispatch enforcer
    /// reads from this so an unknown / future `host.*` method is
    /// rejected at the boundary rather than silently routed to a
    /// dead handler.
    pub fn knows(name: &str) -> bool {
        Self::method_names().any(|m| m == name)
    }
}

/// Outcome of a successful SPEC §4.2.3 `initialize` handshake.
///
/// Stored on [`crate::plugin::proc::PluginProc::caps`] so the Step 6
/// dispatch path can short-circuit `check_cap` for methods the plugin
/// never offered, and Step 7's registry can index plugins by the
/// (kind, mime|url) pairs the plugin actually claimed at handshake
/// time (not just what the manifest declared).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedCaps {
    /// API version both ends agreed on (one of the strings the host
    /// advertised in `SpawnOpts::host_api`, narrowed by the plugin's
    /// `initialize.result.api` choice).
    pub api: String,
    /// The capability set the plugin advertised at `initialize`. Must
    /// be a subset of [`Manifest::capabilities`] — a previewer that
    /// claims `image/png` while its manifest only declares
    /// `text/markdown` is rejected with [`RpcError::Protocol`].
    pub plugin_capabilities: Vec<Capability>,
    /// Host methods the plugin announced it intends to call (the
    /// `initialize.result.host_methods` array). Unknown names are
    /// dropped here and surface as a `tracing::warn!`, not a fatal
    /// error (SPEC §4.1 forward-compat for host-callable namespace).
    pub plugin_offered_host_methods: Vec<String>,
}

/// Build the `params` object for the `initialize` request the host
/// sends at spawn time. Drives the SPEC §4.2.3 handshake input shape.
///
/// `host_methods` reads from [`HostCapabilities::method_names`] so the
/// table is the single source of truth for what the plugin knows is
/// reachable. `workdir` and `cache_dir` are surfaced per the SPEC
/// §4.2.3 `initialize.params.plugin` block.
pub fn build_initialize_params(
    host_name: &str,
    host_version: &str,
    host_api: &[String],
    workdir: &std::path::Path,
) -> serde_json::Value {
    let host_methods: Vec<&'static str> = HostCapabilities::method_names().collect();
    serde_json::json!({
        "host": {
            "name": host_name,
            "version": host_version,
            "api": host_api,
            "capabilities": {},
            "host_methods": host_methods,
        },
        "plugin": {
            "workdir": workdir,
            "cache_dir": workdir.join("cache"),
        },
    })
}

/// Parse the plugin's `initialize` response into a [`NegotiatedCaps`].
///
/// Enforces the SPEC §4.2.3 cross-checks:
///
/// 1. `api` ∈ host's advertised set, else [`API_VERSION_MISMATCH`]
///    surfaces as [`RpcError::Peer { code: -32098, .. }`].
/// 2. Every advertised `Capability` is also in the manifest's
///    `[[capability]]` list, else [`RpcError::Protocol`].
/// 3. Every `host_method` the plugin offers either matches
///    [`HostCapabilities::ALL`] (kept) or is warned-and-dropped
///    (forward-compat).
pub fn parse_initialize_result(
    result: &serde_json::Value,
    manifest: &Manifest,
    host_api: &[String],
) -> std::result::Result<NegotiatedCaps, RpcError> {
    let api = result
        .get("api")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::Handshake("initialize result missing `api` string".into()))?
        .to_string();
    if !host_api.iter().any(|h| h == &api) {
        return Err(RpcError::Peer {
            code: API_VERSION_MISMATCH,
            message: "API_VERSION_MISMATCH".into(),
            data: serde_json::json!({ "plugin_api": api, "host_api": host_api }),
        });
    }

    let plugin_capabilities = parse_capabilities(result)?;
    cross_check_capabilities(&plugin_capabilities, manifest)?;

    let plugin_offered_host_methods = filter_known_host_methods(result, &manifest.plugin.id);

    Ok(NegotiatedCaps {
        api,
        plugin_capabilities,
        plugin_offered_host_methods,
    })
}

/// Deserialise the `capabilities` array from the plugin's `initialize`
/// result into a typed [`Vec<Capability>`]. Malformed entries fail the
/// handshake with [`RpcError::Protocol`] — the SPEC §4.2.3 contract
/// requires this shape verbatim and a silent drop would corrupt the
/// Step 7 registry index.
fn parse_capabilities(
    result: &serde_json::Value,
) -> std::result::Result<Vec<Capability>, RpcError> {
    let Some(arr) = result.get("capabilities") else {
        return Ok(Vec::new());
    };
    let Some(items) = arr.as_array() else {
        return Err(RpcError::Protocol(
            "initialize result `capabilities` must be an array".into(),
        ));
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let cap: Capability = serde_json::from_value(item.clone()).map_err(|e| {
            RpcError::Protocol(format!("initialize capabilities[{i}] malformed: {e}"))
        })?;
        out.push(cap);
    }
    Ok(out)
}

/// Each advertised capability must correspond to a row in the
/// manifest's `[[capability]]` list. A previewer that claims a MIME
/// or URL its manifest never declared is rejected — otherwise a
/// plugin could silently broaden its surface beyond what the user
/// signed for in the manifest.
fn cross_check_capabilities(
    advertised: &[Capability],
    manifest: &Manifest,
) -> std::result::Result<(), RpcError> {
    for cap in advertised {
        if !manifest
            .capabilities
            .iter()
            .any(|m| m.kind == cap.kind && m.url == cap.url && m.mime == cap.mime)
        {
            return Err(RpcError::Protocol(format!(
                "plugin advertised capability not in manifest: kind={kind:?} url={url:?} mime={mime:?}",
                kind = cap.kind,
                url = cap.url,
                mime = cap.mime,
            )));
        }
    }
    Ok(())
}

/// Filter the plugin's `host_methods` (or legacy `offers`) array down
/// to the entries [`HostCapabilities::ALL`] knows about. Unknown names
/// are warned-and-dropped so a plugin built against a future host
/// version still handshakes successfully on this host (SPEC §4.1
/// forward-compat — the same rule that governs unknown TOML keys).
fn filter_known_host_methods(result: &serde_json::Value, plugin_id: &str) -> Vec<String> {
    let raw = result
        .get("host_methods")
        .or_else(|| result.get("offers"))
        .and_then(|v| v.as_array());
    let Some(items) = raw else {
        return Vec::new();
    };
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        let Some(name) = item.as_str() else {
            tracing::warn!(
                target = "sy::plugin::capability",
                plugin_id,
                value = ?item,
                "initialize host_methods entry must be a string; dropping"
            );
            continue;
        };
        if HostCapabilities::knows(name) {
            kept.push(name.to_string());
        } else {
            tracing::warn!(
                target = "sy::plugin::capability",
                plugin_id,
                method = %name,
                "plugin offered unknown host method; dropping (forward-compat)"
            );
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    //! Capability-negotiation behaviour is exercised at two layers:
    //!
    //! * Pure unit tests on [`parse_initialize_result`] cover the four
    //!   SPEC §4.2.3 cross-checks in isolation (api version, capability
    //!   subset, offers forward-compat, malformed response).
    //! * The integration-level scenarios from roadmap Step 5 drive
    //!   [`crate::plugin::proc::spawn`] end-to-end so the wire
    //!   contract — `initialize` request bytes, response parsing,
    //!   `PluginProc::caps` storage — is locked in against a real
    //!   `/bin/bash` stub plugin.
    use super::*;
    use crate::plugin::manifest::load;
    use crate::plugin::proc::{spawn, RpcError, SpawnOpts, State};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// SPEC §4.2.3 `api = "1"`, declares one `text/markdown` previewer.
    /// Used as the baseline manifest for every test in this module —
    /// individual scenarios override only the bits they need.
    const BASELINE_MANIFEST: &str = r#"
api = "1"

[plugin]
id = "sy-plugin-capability-test"
name = "Capability Test"
version = "0.0.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "{exec}"

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
spawn_timeout_ms = 1500
shutdown_timeout_ms = 500

[env]
PATH = "/usr/bin:/bin"
"#;

    /// Render `BASELINE_MANIFEST` with the `{exec}` template field
    /// substituted. The caller passes `api_min`/`api_max` overrides as
    /// `(min, max)` so the version-skew tests stay readable.
    fn render_manifest(exec: &str, api_range: Option<(&str, &str)>) -> String {
        let mut src = BASELINE_MANIFEST.replace("{exec}", exec);
        if let Some((min, max)) = api_range {
            src = src.replace(
                "api_min = \"1\"\napi_max = \"1\"",
                &format!("api_min = \"{min}\"\napi_max = \"{max}\""),
            );
        }
        src
    }

    /// Write a bash stub to disk and chmod 755. Matches the
    /// `proc.rs::tests::write_script` helper byte-for-byte so the
    /// stubs land under the same permissions the supervisor
    /// `tests` apply.
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).expect("write script");
        let mut perms = std::fs::metadata(&p).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).expect("chmod");
        p
    }

    fn opts_for(workdir: &Path) -> SpawnOpts {
        let mut o = SpawnOpts::new(workdir.to_path_buf());
        o.ping_interval = Duration::from_millis(80);
        o.ping_timeout = Duration::from_millis(300);
        o.request_timeout = Duration::from_secs(2);
        o
    }

    /// Build a bash plugin script whose `initialize` reply is the
    /// caller-supplied `result_body` (a JSON object encoded as a
    /// string). Loops responding to `shutdown` / `ping` so the
    /// supervisor can drive the full lifecycle.
    fn stub_with_initialize_result(result_body: &str) -> String {
        format!(
            r#"#!/bin/bash
emit() {{
  local body="$1"
  printf 'Content-Length: %d\r\n\r\n%s' "${{#body}}" "$body"
}}
FRAME=""
read_frame() {{
  local len=0 line
  while IFS= read -r line; do
    line="${{line%$'\r'}}"
    [ -z "$line" ] && break
    case "$line" in
      Content-Length:*)
        len="${{line#Content-Length: }}"
        len="${{len// /}}"
        ;;
    esac
  done || {{ FRAME=""; return 1; }}
  if [ "$len" -gt 0 ]; then
    FRAME=$(dd bs=1 count="$len" 2>/dev/null)
  else
    FRAME=""
  fi
  return 0
}}
read_frame
emit '{{"jsonrpc":"2.0","id":1,"result":{result_body}}}'
while read_frame; do
  [ -z "$FRAME" ] && break
  case "$FRAME" in
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$FRAME" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      emit "{{\"jsonrpc\":\"2.0\",\"id\":${{id}},\"result\":null}}"
      read_frame
      break
      ;;
    *'"method":"ping"'*)
      id=$(printf '%s' "$FRAME" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      ts=$(printf '%s' "$FRAME" | sed -n 's/.*"ts":\([0-9]*\).*/\1/p')
      emit "{{\"jsonrpc\":\"2.0\",\"id\":${{id}},\"result\":{{\"ts\":${{ts}}}}}}"
      ;;
  esac
done
exit 0
"#,
            result_body = result_body
        )
    }

    /// SPEC §4.2.3 happy path — host advertises `["1"]`, manifest
    /// declares `api_min = api_max = "1"`, plugin replies with
    /// `api = "1"` and the same `previewer / text/markdown`
    /// capability as the manifest. Handshake completes and the
    /// supervisor exposes `State::Ready` plus a `NegotiatedCaps` whose
    /// `api` field is `"1"`.
    #[tokio::test(flavor = "current_thread")]
    async fn matching_api_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        let body = r#"{"name":"cap-stub","version":"0","api":"1","capabilities":[{"kind":"previewer","mime":"text/markdown"}],"host_methods":[]}"#;
        let script = write_script(tmp.path(), "cap-ok.sh", &stub_with_initialize_result(body));
        let m = load(&render_manifest(&script.to_string_lossy(), None)).expect("manifest");
        let mut proc = spawn(m, opts_for(tmp.path())).await.expect("spawn");
        assert_eq!(proc.health(), State::Ready);
        let caps = proc.caps().expect("caps stored on PluginProc");
        assert_eq!(caps.api, "1");
        assert_eq!(caps.plugin_capabilities.len(), 1);
        assert_eq!(caps.plugin_capabilities[0].kind, "previewer");
        let _ = proc.shutdown().await;
    }

    /// SPEC §4.2.3 api-skew — host advertises `["1"]`, manifest
    /// declares `api_min = "2"` (deserialise-time validation rejects
    /// the manifest itself because `m.api = "1"` falls outside
    /// `[2,3]`). Drives the *out-of-set* path: a runtime plugin whose
    /// `initialize` reply uses `api = "2"` against a host that only
    /// advertises `["1"]` — handshake must surface
    /// [`RpcError::Peer { code: -32098, .. }`].
    #[tokio::test(flavor = "current_thread")]
    async fn api_mismatch_returns_32098() {
        let tmp = tempfile::tempdir().expect("tmp");
        // Plugin replies with api = "2" even though the host advertises ["1"].
        let body =
            r#"{"name":"cap-skew","version":"0","api":"2","capabilities":[],"host_methods":[]}"#;
        let script = write_script(
            tmp.path(),
            "cap-skew.sh",
            &stub_with_initialize_result(body),
        );
        // Manifest itself stays valid (api = "1" ∈ [1,1]).
        let m = load(&render_manifest(&script.to_string_lossy(), None)).expect("manifest");
        let err = spawn(m, opts_for(tmp.path()))
            .await
            .expect_err("api skew must fail spawn");
        match err {
            RpcError::Peer { code, .. } => {
                assert_eq!(code, API_VERSION_MISMATCH, "must surface -32098");
            }
            other => panic!("expected Peer(-32098), got {other:?}"),
        }
    }

    /// SPEC §4.2.3 capability subset — plugin advertises an
    /// `image/png` previewer at handshake but its manifest only
    /// declared `text/markdown`. Reject with a stable
    /// [`RpcError::Protocol`] so the file manager never routes a
    /// hover to a previewer the user didn't sign for.
    #[tokio::test(flavor = "current_thread")]
    async fn plugin_capabilities_must_match_manifest() {
        let tmp = tempfile::tempdir().expect("tmp");
        // Plugin claims image/png — but manifest only declares text/markdown.
        let body = r#"{"name":"cap-overreach","version":"0","api":"1","capabilities":[{"kind":"previewer","mime":"image/png"}],"host_methods":[]}"#;
        let script = write_script(
            tmp.path(),
            "cap-overreach.sh",
            &stub_with_initialize_result(body),
        );
        let m = load(&render_manifest(&script.to_string_lossy(), None)).expect("manifest");
        let err = spawn(m, opts_for(tmp.path()))
            .await
            .expect_err("capability overreach must fail");
        match err {
            RpcError::Protocol(msg) => assert!(
                msg.contains("not in manifest"),
                "expected 'not in manifest' message, got: {msg}"
            ),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    /// SPEC §4.1 forward-compat — plugin offers `host.future_api`,
    /// which [`HostCapabilities::ALL`] does not list. Handshake must
    /// succeed (the rule mirrors the unknown-TOML-key rule), the
    /// unknown name must be dropped from the stored
    /// `NegotiatedCaps`, and a `tracing::warn!` must fire so the
    /// operator can see the drift.
    #[tokio::test(flavor = "current_thread")]
    async fn offers_unknown_method_is_warned_not_fatal() {
        let tmp = tempfile::tempdir().expect("tmp");
        let body = r#"{"name":"cap-future","version":"0","api":"1","capabilities":[{"kind":"previewer","mime":"text/markdown"}],"host_methods":["host.fs.read","host.future_api"]}"#;
        let script = write_script(
            tmp.path(),
            "cap-future.sh",
            &stub_with_initialize_result(body),
        );
        let m = load(&render_manifest(&script.to_string_lossy(), None)).expect("manifest");
        let mut proc = spawn(m, opts_for(tmp.path()))
            .await
            .expect("forward-compat must not fail spawn");
        let caps = proc.caps().expect("caps stored");
        assert_eq!(
            caps.plugin_offered_host_methods,
            vec!["host.fs.read".to_string()],
            "unknown host method must be dropped from NegotiatedCaps"
        );
        let _ = proc.shutdown().await;
    }

    /// Unit-test the pure [`parse_initialize_result`] helper for the
    /// happy path — locks the parse contract in independent of the
    /// process-spawn integration tests above. Future steps that
    /// build on `NegotiatedCaps` re-use this helper directly.
    #[test]
    fn parse_initialize_result_round_trip() {
        let raw = render_manifest("/bin/true", None);
        let m = load(&raw).expect("manifest");
        let result = serde_json::json!({
            "name": "unit-stub",
            "version": "0",
            "api": "1",
            "capabilities": [{"kind": "previewer", "mime": "text/markdown"}],
            "host_methods": ["host.fs.read", "host.notify.waybar"],
        });
        let host_api = vec!["1".to_string()];
        let caps = parse_initialize_result(&result, &m, &host_api).expect("parse ok");
        assert_eq!(caps.api, "1");
        assert_eq!(caps.plugin_capabilities.len(), 1);
        assert_eq!(caps.plugin_offered_host_methods.len(), 2);
    }

    /// Lock the public list of host capabilities in. Step 5 fixed the
    /// first seven; Step 27 added `host.preview.image_show` +
    /// `host.preview.text` for the J3 plugin-routed previewer
    /// pipeline. Later steps that add host fns extend this assertion
    /// (RoadMap §6 deferred set).
    #[test]
    fn host_capabilities_all_lists_step_6_plus_step_27() {
        let names: Vec<&'static str> = HostCapabilities::method_names().collect();
        assert_eq!(
            names,
            vec![
                "host.fs.read",
                "host.fs.cha",
                "host.fs.write_cache",
                "host.notify.waybar",
                "host.notify.banner",
                "host.ui.theme",
                "host.exec.run",
                "host.preview.image_show",
                "host.preview.text",
            ],
            "HostCapabilities::ALL must match the SPEC §4.2.5 rows landing in Steps 6 + 27"
        );
    }
}
