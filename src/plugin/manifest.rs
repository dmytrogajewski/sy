//! `plugin.toml` manifest parser + validator.
//!
//! Implements the grammar from [plugin SPEC
//! §4.1](../../../specs/research/sy-file-manager-plugins/SPEC.md#41-manifest-grammar-plugintoml).
//!
//! Pure functions over `&str`; no I/O. Unknown keys warn (via
//! `tracing::warn`) but never fail the parse — forward compatibility
//! per SPEC §4.1 closing paragraph ("lints unknown keys (warn, don't
//! fail — forward compatibility)").
//!
//! Predicates (`url`, `mime`) are validated by compiling each glob
//! through `globset::Glob` so the parser rejects malformed patterns
//! at load time rather than at dispatch time. The choice of `globset`
//! over `regex-lite` is recorded in the roadmap step 1 risks block
//! (`specs/roadmaps/sy-file-manager/ROADMAP.md`) — glob semantics
//! match the predicate language and the dep is already pinned at the
//! workspace level.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, Context, Result};
use globset::Glob;
use serde::{Deserialize, Serialize};

/// Parsed `plugin.toml` manifest (SPEC §4.1).
///
/// Constructed via [`parse`]; validated via [`validate`]. Construct +
/// validate in one shot with [`load`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Manifest {
    /// Plugin contract version. Host accepts `"1"` today (SPEC §4.1).
    pub api: String,
    /// Plugin identity block (SPEC §4.1 `[plugin]`).
    pub plugin: PluginMeta,
    /// Offered capabilities (SPEC §4.1 `[[capability]]`).
    #[serde(default, rename = "capability")]
    pub capabilities: Vec<Capability>,
    /// Host-fn needs (SPEC §4.1 `[needs]`).
    #[serde(default)]
    pub needs: Needs,
    /// Resource limits (SPEC §4.1 `[limits]`).
    #[serde(default)]
    pub limits: Limits,
    /// Optional env-var overrides for the plugin process (SPEC §4.1
    /// `[env]`).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional minisign signature block (SPEC §4.1
    /// `[plugin.signature]`). Lifted from `plugin.signature` to the
    /// top level after parse so consumers don't have to drill through
    /// the meta table.
    #[serde(skip)]
    pub signature: Option<Signature>,
}

/// Plugin identity + binary location (SPEC §4.1 `[plugin]` +
/// `[plugin.binary]`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginMeta {
    /// Kebab-case unique plugin id (SPEC §4.1).
    pub id: String,
    /// Human-readable name (SPEC §4.1).
    pub name: String,
    /// SemVer-ish plugin version (SPEC §4.1).
    pub version: String,
    /// One-line description (SPEC §4.1).
    #[serde(default)]
    pub description: String,
    /// Authors (SPEC §4.1).
    #[serde(default)]
    pub authors: Vec<String>,
    /// SPDX license expression (SPEC §4.1).
    #[serde(default)]
    pub license: String,
    /// Project homepage URL (SPEC §4.1).
    #[serde(default)]
    pub homepage: String,
    /// Minimum host API version this plugin supports (SPEC §4.1).
    pub api_min: String,
    /// Maximum host API version this plugin supports (SPEC §4.1).
    pub api_max: String,
    /// Binary spec (SPEC §4.1 `[plugin.binary]`).
    pub binary: BinarySpec,
    /// Optional signature block (SPEC §4.1 `[plugin.signature]`).
    /// Lifted to [`Manifest::signature`] after parse.
    #[serde(default)]
    pub signature: Option<Signature>,
}

/// Plugin binary spec (SPEC §4.1 `[plugin.binary]`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct BinarySpec {
    /// Path to the executable, relative to the manifest directory
    /// (SPEC §4.1).
    pub exec: String,
    /// Optional pre-spawn commands; host runs each and aborts on
    /// non-zero (SPEC §4.1).
    #[serde(default)]
    pub preflight: Vec<String>,
}

/// Optional minisign signature block (SPEC §4.1
/// `[plugin.signature]`). Verified at install + on every spawn
/// (mtime-cached) by later roadmap steps.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Signature {
    /// Base64-encoded minisign signature over the binary + manifest.
    pub sig: String,
    /// Minisign public key or `configs/sy/plugin-publishers/<name>.pub`
    /// reference.
    pub pubkey: String,
}

/// A single offered capability (SPEC §4.1 `[[capability]]`).
///
/// At least one of `url` or `mime` must be present; both are
/// glob-shaped patterns enforced via `globset::Glob`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Capability {
    /// Capability kind: `"previewer"`, `"action"`, `"opener"`,
    /// `"fetcher"`, `"indexer"` (SPEC §4.1 / §4.2.4).
    pub kind: String,
    /// Optional url-glob predicate (SPEC §4.1, e.g. `*.md`).
    #[serde(default)]
    pub url: Option<String>,
    /// Optional mime-glob predicate (SPEC §4.1, e.g. `text/*`).
    #[serde(default)]
    pub mime: Option<String>,
}

impl Capability {
    /// Compile the `url` predicate to a [`globset::Glob`].
    ///
    /// Returns `Ok(None)` if no `url` predicate was declared.
    pub fn url_glob(&self) -> Result<Option<Glob>> {
        match &self.url {
            None => Ok(None),
            Some(s) => Ok(Some(
                Glob::new(s).with_context(|| format!("capability url glob {s:?}"))?,
            )),
        }
    }

    /// Compile the `mime` predicate to a [`globset::Glob`].
    ///
    /// Returns `Ok(None)` if no `mime` predicate was declared.
    pub fn mime_glob(&self) -> Result<Option<Glob>> {
        match &self.mime {
            None => Ok(None),
            Some(s) => Ok(Some(
                Glob::new(s).with_context(|| format!("capability mime glob {s:?}"))?,
            )),
        }
    }
}

/// Host functions the plugin asks for (SPEC §4.1 `[needs]`).
///
/// Host enforces by inspecting these lists at every `host.*` RPC
/// call; an undeclared capability fails the call with
/// `-32099 / CAP_NOT_GRANTED` (SPEC §4.2.2).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Needs {
    /// Path scopes the plugin may `host.fs.read` (SPEC §4.1).
    #[serde(default)]
    pub fs_read: Vec<String>,
    /// Cache slots the plugin may `host.fs.write_cache` (SPEC §4.1).
    #[serde(default)]
    pub fs_write: Vec<String>,
    /// Preview-namespace host fns the plugin may call (SPEC §4.1).
    #[serde(default)]
    pub preview: Vec<String>,
    /// Knowledge-namespace host fns (SPEC §4.1; empty = none).
    #[serde(default)]
    pub knowledge: Vec<String>,
    /// Outbound network destinations (SPEC §4.1; empty = none).
    #[serde(default)]
    pub network: Vec<String>,
    /// Subprocess allow-list (SPEC §4.1, e.g. `["pdftoppm"]`).
    #[serde(default)]
    pub exec: Vec<String>,
}

/// Resource limits (SPEC §4.1 `[limits]`).
///
/// All fields are `u32`; the TOML parser rejects negative values at
/// load time, which is what the `rejects_negative_limits` test
/// exercises.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Limits {
    /// `RLIMIT_AS` ceiling in MiB (SPEC §4.1 / §4.3).
    pub memory_mb: u32,
    /// `RLIMIT_CPU` ceiling in seconds (SPEC §4.1 / §4.3).
    pub cpu_seconds: u32,
    /// `RLIMIT_NOFILE` ceiling (SPEC §4.1 / §4.3).
    pub nofile: u32,
    /// Host wait budget before considering spawn failed (SPEC §4.1).
    pub spawn_timeout_ms: u32,
    /// Host wait budget after `shutdown` before sending `exit`
    /// (SPEC §4.1).
    pub shutdown_timeout_ms: u32,
}

impl Default for Limits {
    fn default() -> Self {
        // Defaults match SPEC §4.1 example. Used when `[limits]` is
        // omitted entirely; `validate` still rejects zero values so
        // the implicit defaults never silently mask a degenerate
        // manifest.
        Self {
            memory_mb: 64,
            cpu_seconds: 30,
            nofile: 64,
            spawn_timeout_ms: 250,
            shutdown_timeout_ms: 1000,
        }
    }
}

/// Parse a `plugin.toml` body into a [`Manifest`].
///
/// Unknown top-level keys are warned about via `tracing::warn!` and
/// preserved-by-ignoring per SPEC §4.1 closing paragraph (forward
/// compatibility). Use [`validate`] to run the semantic checks.
///
/// Returns `Err` only on TOML syntax errors and required-field
/// absence; semantic mismatches surface from [`validate`].
pub fn parse(src: &str) -> Result<Manifest> {
    // First pass: surface unknown top-level keys as `tracing::warn!`
    // without aborting the parse (SPEC §4.1 forward-compat rule).
    let raw: toml::Value = toml::from_str(src).context("plugin.toml: invalid TOML")?;
    if let Some(tbl) = raw.as_table() {
        warn_unknown_top_level(tbl);
    }

    // Second pass: typed deserialise. `serde(deny_unknown_fields)` is
    // intentionally NOT used — unknowns are warned about above, not
    // rejected here.
    let mut manifest: Manifest =
        toml::from_str(src).context("plugin.toml: deserialise into Manifest")?;
    // Lift the signature block to the top level for ergonomic access.
    manifest.signature = manifest.plugin.signature.clone();
    Ok(manifest)
}

/// Validate a parsed [`Manifest`] for semantic well-formedness.
///
/// Checks (SPEC §4.1 + §4.2.3):
/// * `api` is non-empty and falls within `api_min..=api_max`.
/// * Every `[[capability]]` declares at least one of `url`/`mime`,
///   and both predicates compile as `globset::Glob` patterns.
/// * Every `[limits]` field is strictly positive (rlimit `0` would
///   kill the child immediately).
pub fn validate(m: &Manifest) -> Result<()> {
    if m.api.is_empty() {
        return Err(anyhow!("plugin.toml: top-level `api` is required"));
    }
    let api_min = &m.plugin.api_min;
    let api_max = &m.plugin.api_max;
    if !(api_min.as_str() <= m.api.as_str() && m.api.as_str() <= api_max.as_str()) {
        return Err(anyhow!(
            "plugin.toml: api={api} outside [api_min={min}, api_max={max}]",
            api = m.api,
            min = api_min,
            max = api_max,
        ));
    }
    if m.plugin.id.is_empty() {
        return Err(anyhow!("plugin.toml: [plugin] id is required"));
    }
    if m.plugin.binary.exec.is_empty() {
        return Err(anyhow!("plugin.toml: [plugin.binary] exec is required"));
    }
    for (i, cap) in m.capabilities.iter().enumerate() {
        if cap.kind.is_empty() {
            return Err(anyhow!("plugin.toml: capability[{i}] kind is required"));
        }
        if cap.url.is_none() && cap.mime.is_none() {
            return Err(anyhow!(
                "plugin.toml: capability[{i}] needs at least one of url/mime"
            ));
        }
        cap.url_glob()
            .with_context(|| format!("plugin.toml: capability[{i}] url glob"))?;
        cap.mime_glob()
            .with_context(|| format!("plugin.toml: capability[{i}] mime glob"))?;
    }
    validate_limits(&m.limits)?;
    Ok(())
}

/// Parse + validate in one shot. Convenience for call sites that
/// don't want to thread two `Result`s.
pub fn load(src: &str) -> Result<Manifest> {
    let m = parse(src)?;
    validate(&m)?;
    Ok(m)
}

fn validate_limits(l: &Limits) -> Result<()> {
    // Zero is as bad as negative on rlimits — the kernel will SIGKILL
    // the child the moment it allocates a page or opens stdin. SPEC
    // §4.1 example shows positive non-zero values throughout.
    let pairs: [(&str, u32); 5] = [
        ("memory_mb", l.memory_mb),
        ("cpu_seconds", l.cpu_seconds),
        ("nofile", l.nofile),
        ("spawn_timeout_ms", l.spawn_timeout_ms),
        ("shutdown_timeout_ms", l.shutdown_timeout_ms),
    ];
    for (name, v) in pairs {
        if v == 0 {
            return Err(anyhow!("plugin.toml: [limits] {name} must be > 0"));
        }
    }
    Ok(())
}

/// Top-level keys recognised by [`Manifest`]. Anything outside this
/// set surfaces as a `tracing::warn!` per SPEC §4.1 forward-compat.
fn known_top_level_keys() -> BTreeSet<&'static str> {
    ["api", "plugin", "capability", "needs", "limits", "env"]
        .into_iter()
        .collect()
}

fn warn_unknown_top_level(tbl: &toml::value::Table) {
    let known = known_top_level_keys();
    for key in tbl.keys() {
        if !known.contains(key.as_str()) {
            tracing::warn!(
                target = "sy::plugin::manifest",
                key = %key,
                "plugin.toml: ignoring unknown top-level key (forward compatibility)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §4.1 grammar block verbatim. Kept as a single `const` so
    /// any drift between this fixture and the SPEC is easy to diff.
    const SPEC_4_1_CANONICAL: &str = r#"
api = "1"

[plugin]
id = "sy-plugin-md"
name = "Markdown Previewer"
version = "0.1.0"
description = "Renders Markdown to PNG via pulldown-cmark + cosmic-text"
authors = ["Dmitriy Gajewski <…>"]
license = "Apache-2.0"
homepage = "https://github.com/dmytrogajewski/sy"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "./bin/sy-plugin-md"
preflight = ["./bin/sy-plugin-md", "--check"]

[plugin.signature]
sig = "<base64 minisig>"
pubkey = "RWT8…"

[[capability]]
kind = "previewer"
url = "*.md"
[[capability]]
kind = "previewer"
url = "*.markdown"
[[capability]]
kind = "previewer"
mime = "text/markdown"

[needs]
fs_read = ["arg.path"]
fs_write = ["cache"]
preview = ["image_show"]
knowledge = []
network = []
exec = []

[limits]
memory_mb = 64
cpu_seconds = 30
nofile = 64
spawn_timeout_ms = 250
shutdown_timeout_ms = 1000

[env]
RUST_LOG = "info"
"#;

    #[test]
    fn parses_canonical_example() {
        let m =
            load(SPEC_4_1_CANONICAL).expect("canonical SPEC §4.1 fixture must parse + validate");

        assert_eq!(m.api, "1");
        assert_eq!(m.plugin.id, "sy-plugin-md");
        assert_eq!(m.plugin.name, "Markdown Previewer");
        assert_eq!(m.plugin.version, "0.1.0");
        assert_eq!(m.plugin.api_min, "1");
        assert_eq!(m.plugin.api_max, "1");
        assert_eq!(m.plugin.binary.exec, "./bin/sy-plugin-md");
        assert_eq!(
            m.plugin.binary.preflight,
            vec!["./bin/sy-plugin-md".to_string(), "--check".to_string()]
        );

        assert_eq!(m.capabilities.len(), 3);
        assert_eq!(m.capabilities[0].kind, "previewer");
        assert_eq!(m.capabilities[0].url.as_deref(), Some("*.md"));
        assert_eq!(m.capabilities[2].mime.as_deref(), Some("text/markdown"));

        assert_eq!(m.needs.fs_read, vec!["arg.path".to_string()]);
        assert_eq!(m.needs.fs_write, vec!["cache".to_string()]);
        assert_eq!(m.needs.preview, vec!["image_show".to_string()]);
        assert!(m.needs.knowledge.is_empty());
        assert!(m.needs.network.is_empty());
        assert!(m.needs.exec.is_empty());

        assert_eq!(m.limits.memory_mb, 64);
        assert_eq!(m.limits.cpu_seconds, 30);
        assert_eq!(m.limits.nofile, 64);
        assert_eq!(m.limits.spawn_timeout_ms, 250);
        assert_eq!(m.limits.shutdown_timeout_ms, 1000);

        assert_eq!(m.env.get("RUST_LOG").map(String::as_str), Some("info"));

        let sig = m.signature.as_ref().expect("signature block present");
        assert_eq!(sig.sig, "<base64 minisig>");
        assert_eq!(sig.pubkey, "RWT8…");
    }

    #[test]
    fn rejects_missing_api_version() {
        // Drop the top-level `api = "1"` line — TOML still parses,
        // but typed deserialise fails on the required field.
        let src = SPEC_4_1_CANONICAL.replace("api = \"1\"\n", "");
        let err = parse(&src).expect_err("missing top-level api must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("api") || msg.contains("missing field"),
            "error should mention the missing api field, got: {msg}"
        );
    }

    #[test]
    fn warns_on_unknown_key_but_succeeds() {
        // Append a future-version top-level key. Per SPEC §4.1 the
        // parser must accept it and validate must succeed.
        let src = format!("{SPEC_4_1_CANONICAL}\nfuture_field = \"reserved\"\n");
        let m = load(&src).expect("unknown top-level key must not fail the parse");
        assert_eq!(m.plugin.id, "sy-plugin-md");
    }

    #[test]
    fn glob_predicates_compile() {
        // `url = "*.md"` and `mime = "text/*"` from the canonical
        // fixture must round-trip through globset::Glob.
        let m = load(SPEC_4_1_CANONICAL).expect("fixture parses");
        let md_url = m.capabilities[0]
            .url_glob()
            .expect("md url glob compiles")
            .expect("url predicate present");
        let matcher = md_url.compile_matcher();
        assert!(matcher.is_match("README.md"));
        assert!(!matcher.is_match("README.rst"));

        // Validate the wildcard-mime variant the SPEC text mentions
        // alongside the canonical fixture ("mime-glob, e.g. text/*").
        let src = SPEC_4_1_CANONICAL.replace("mime = \"text/markdown\"", "mime = \"text/*\"");
        let m2 = load(&src).expect("wildcard mime fixture parses");
        let mime_g = m2.capabilities[2]
            .mime_glob()
            .expect("mime glob compiles")
            .expect("mime predicate present");
        let mm = mime_g.compile_matcher();
        assert!(mm.is_match("text/markdown"));
        assert!(mm.is_match("text/plain"));
        assert!(!mm.is_match("image/png"));
    }

    #[test]
    fn rejects_negative_limits() {
        // `memory_mb = -1` is the canonical "negative limit" the
        // SPEC §4.1 grammar forbids. The TOML parser rejects it
        // because the typed field is `u32`; the error must be
        // surfaced from `parse`.
        let src = SPEC_4_1_CANONICAL.replace("memory_mb = 64", "memory_mb = -1");
        let err = parse(&src).expect_err("negative memory_mb must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid value")
                || msg.contains("out of range")
                || msg.contains("invalid type"),
            "error should mention the bad limit, got: {msg}"
        );

        // Zero is also forbidden — `validate` catches it because
        // `u32` accepts `0` but rlimit 0 is unusable. Belt-and-
        // braces check that both negative *and* zero fail.
        let src0 = SPEC_4_1_CANONICAL.replace("memory_mb = 64", "memory_mb = 0");
        let err0 = load(&src0).expect_err("zero memory_mb must fail validation");
        let msg0 = format!("{err0:#}");
        assert!(msg0.contains("memory_mb"), "got: {msg0}");
    }
}
