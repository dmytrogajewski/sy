//! `sy plugin` CLI surface — Step 8 of the
//! [`sy-file-manager` roadmap][roadmap]. Implements
//! [plugin SPEC §4.5][spec-cli] minus the git-URL `install` flow,
//! which lands in Step 9. This is the first non-test bin consumer of
//! [`crate::plugin`] — Step 1's `#[cfg(test)] mod plugin;` gate in
//! `src/main.rs` comes off because `Cmd::Plugin` reaches every other
//! submodule at runtime through this dispatcher.
//!
//! **Exit codes** (mirrors SPEC §4.5):
//!
//! | Code | Meaning                                                  |
//! |------|----------------------------------------------------------|
//! | 0    | ok                                                       |
//! | 1    | generic                                                  |
//! | 2    | usage / validation (bad args, bad glob, malformed TOML)  |
//! | 3    | drift detected (reserved — Step 9+)                      |
//! | 6    | manifest invalid (reserved — Step 9)                     |
//! | 7    | signature mismatch (reserved — Step 9)                   |
//! | 8    | plugin unreachable / unhealthy                           |
//!
//! **`--json` schema** (forward-compat contract — Step 35 mirrors this in
//! `docs/reference/sy-file-mcp.md`):
//!
//! * `sy plugin list --json` → `{"schema":"sy.plugin.list/v1","plugins":[{"id","version","name","capabilities":[{"kind","mime?","url?"}]}]}`.
//! * `sy plugin doctor --json` → `{"schema":"sy.plugin.doctor/v1","checks":[{"plugin","name","ok","detail"}]}`.
//!
//! [roadmap]: ../../../specs/roadmaps/sy-file-manager/ROADMAP.md
//! [spec-cli]: ../../../specs/research/sy-file-manager-plugins/SPEC.md#45-cli--mcp-surface
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::plugin::host_fns;
use crate::plugin::install::{self, InstallOpts, InstallSource};
use crate::plugin::manifest::{self, Manifest};
use crate::plugin::proc::{self, RpcError, SpawnOpts};
use crate::plugin::registry::{self, CapKind, PluginId, Registry};
use crate::plugin::rpc as wire_rpc;
use crate::plugin::sandbox;

/// SPEC §4.5 stable exit code: usage / validation error (bad args,
/// bad glob, malformed TOML). Mirrors SPEC §4.5 row 2.
pub const EXIT_USAGE: i32 = 2;

/// SPEC §4.5 stable exit code: plugin unreachable or unhealthy. The
/// `doctor` subcommand emits this when any check fails (binary
/// missing, manifest references a non-existent path, …). Mirrors
/// SPEC §4.5 row 8.
pub const EXIT_PLUGIN_UNHEALTHY: i32 = 8;

/// SPEC §4.5 stable exit code: signature verification failed at
/// install time (missing block when required, malformed minisign
/// encoding, or signature did not verify against the binary +
/// manifest payload). Mirrors SPEC §4.5 row 7.
pub const EXIT_SIGNATURE_INVALID: i32 = 7;

/// SPEC §4.5 stable exit code: manifest TOML failed parse or
/// validation during install. Mirrors SPEC §4.5 row 6.
pub const EXIT_MANIFEST_INVALID: i32 = 6;

/// `--json` schema marker for `sy plugin list`. Forward-compat
/// contract — Step 35 mirrors this in the docs.
const SCHEMA_LIST: &str = "sy.plugin.list/v1";

/// `--json` schema marker for `sy plugin doctor`.
const SCHEMA_DOCTOR: &str = "sy.plugin.doctor/v1";

/// `sy plugin` subcommand tree.
///
/// Mirrors SPEC §4.5. The git-URL form of `install` is deferred to
/// Step 9 — Step 8 accepts a local path only via the same subcommand
/// surface (not landed in this scaffold; `install` is unmapped today).
#[derive(Debug, Subcommand)]
pub enum PluginCmd {
    /// List discovered plugins from `$SY_PLUGIN_DIR` (or the default
    /// XDG lanes). `--json` emits the `sy.plugin.list/v1` schema.
    ///
    /// Example:
    ///   sy plugin list --json
    List {
        /// Emit the `sy.plugin.list/v1` JSON schema on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Enable a previously-disabled plugin (removes its id from
    /// `$XDG_STATE_HOME/sy/plugin/disabled.toml`). Idempotent.
    Enable {
        /// Plugin id (matches `[plugin] id` from the manifest).
        id: String,
    },
    /// Disable a plugin without uninstalling it. Adds its id to the
    /// disabled-list TOML. Idempotent.
    Disable {
        /// Plugin id (matches `[plugin] id` from the manifest).
        id: String,
    },
    /// Run health checks against every discovered plugin: manifest
    /// well-formed, declared binary exists + is executable, capability
    /// predicates compile. Exits 8 if any check fails.
    ///
    /// Example:
    ///   sy plugin doctor --json
    Doctor {
        /// Emit the `sy.plugin.doctor/v1` JSON schema on stdout.
        #[arg(long)]
        json: bool,
    },
    /// One-shot JSON-RPC against a plugin: spawn, handshake, send one
    /// request, capture the response, exit. The captured `result` is
    /// written to stdout as JSON.
    ///
    /// Example:
    ///   sy plugin exec sample preview --params '{"path":"README.md"}'
    Exec {
        /// Plugin id (matches `[plugin] id` from the manifest).
        id: String,
        /// JSON-RPC method to invoke (e.g. `preview`, `ping`).
        method: String,
        /// JSON-encoded `params` object passed to the plugin. Default
        /// is `null`.
        #[arg(long)]
        params: Option<String>,
    },
    /// Print the raw `plugin.toml` for a discovered plugin to stdout.
    CatManifest {
        /// Plugin id (matches `[plugin] id` from the manifest).
        id: String,
    },
    /// Parse + validate a `plugin.toml` from a path. Exits 2 on any
    /// validation error (malformed TOML, bad glob, missing required
    /// field).
    Validate {
        /// Path to a `plugin.toml` file.
        path: PathBuf,
    },
    /// Re-scan the discovery roots and rebuild the in-memory registry.
    /// Today this is a no-op for the CLI process (each invocation
    /// already re-scans on startup); when the file-manager daemon
    /// lands (Step 13+) it'll send the daemon a signal to refresh.
    Reload,
    /// Install a plugin from a local path or git URL.
    ///
    /// `<source>` is either:
    ///   * a directory containing `plugin.toml` (`./my-plugin`,
    ///     `/abs/path/to/my-plugin`)
    ///   * a git URL prefixed `git+` (`git+https://example.com/p.git`,
    ///     `git+file:///abs/path/to/bare-repo`)
    ///
    /// Signature is verified via minisign-verify unless `--unsigned`.
    /// Lands the plugin atomically under
    /// `$XDG_DATA_HOME/sy/plugins/<id>/`.
    ///
    /// Example:
    ///   sy plugin install ./crates/sy-plugin-md
    ///   sy plugin install `git+https://github.com/example/sy-plugin-foo.git` --rev v0.1.0
    Install {
        /// Local path or `git+<url>` source.
        source: String,
        /// Bypass signature verification (local development only).
        #[arg(long)]
        unsigned: bool,
        /// Git ref / commit to check out when `<source>` is a git
        /// URL. Ignored for path sources.
        #[arg(long)]
        rev: Option<String>,
    },
    /// Uninstall a plugin by id. Removes
    /// `$XDG_DATA_HOME/sy/plugins/<id>/` and is idempotent — exits 0
    /// even if no such plugin is installed.
    Uninstall {
        /// Plugin id (matches `[plugin] id` from the manifest).
        id: String,
    },
}

/// Dispatch a parsed [`PluginCmd`]. The bin's `main.rs` calls this
/// directly; subcommands that need to fail with a specific SPEC §4.5
/// exit code call [`std::process::exit`] inline rather than threading
/// the code through `anyhow::Error` (the existing `PowerError`
/// pattern is reserved for the daemon-driven subsystems).
pub fn dispatch(cmd: PluginCmd) -> Result<()> {
    // SPEC §4.2.2 reserved error-code references — pinned at compile
    // time so a future SPEC revision can't silently re-number the
    // wire-side constants without breaking the build. Step 9 (signature
    // verify) will consume `BAD_PREDICATE` / `RLIMIT_BREACH`
    // / `FRAME_TOO_LARGE` from the bin; today the anti-dead-code probe
    // is the only bin consumer.
    let _reserved_codes: [i32; 4] = [
        wire_rpc::RLIMIT_BREACH,
        wire_rpc::LIMIT_EXCEEDED,
        wire_rpc::BAD_PREDICATE,
        wire_rpc::FRAME_TOO_LARGE,
    ];
    match cmd {
        PluginCmd::List { json } => list_cmd(json),
        PluginCmd::Enable { id } => enable_cmd(&id),
        PluginCmd::Disable { id } => disable_cmd(&id),
        PluginCmd::Doctor { json } => doctor_cmd(json),
        PluginCmd::Exec { id, method, params } => exec_cmd(&id, &method, params.as_deref()),
        PluginCmd::CatManifest { id } => cat_manifest_cmd(&id),
        PluginCmd::Validate { path } => validate_cmd(&path),
        PluginCmd::Reload => reload_cmd(),
        PluginCmd::Install {
            source,
            unsigned,
            rev,
        } => install_cmd(&source, unsigned, rev.as_deref()),
        PluginCmd::Uninstall { id } => uninstall_cmd(&id),
    }
}

/// Emit the `sy.plugin.list/v1` schema on stdout when `--json`;
/// otherwise a human-readable two-column table.
fn list_cmd(json_out: bool) -> Result<()> {
    let reg = registry::discover().context("discover plugins")?;
    if json_out {
        let plugins: Vec<Value> = reg
            .plugin_ids()
            .filter_map(|id| reg.manifest(id).map(|m| manifest_to_json(id, m)))
            .collect();
        let doc = json!({
            "schema": SCHEMA_LIST,
            "plugins": plugins,
        });
        println!("{}", serde_json::to_string(&doc)?);
        return Ok(());
    }
    for id in reg.plugin_ids() {
        let Some(m) = reg.manifest(id) else {
            continue;
        };
        println!(
            "{:<32} {}  {}",
            id.as_str(),
            m.plugin.version,
            m.plugin.name
        );
    }
    Ok(())
}

/// Serialise one manifest into the `sy.plugin.list/v1` plugin entry
/// shape. Kept separate from the typed `Manifest` so the JSON schema
/// can evolve without touching the parser.
fn manifest_to_json(id: &PluginId, m: &Manifest) -> Value {
    let caps: Vec<Value> = m
        .capabilities
        .iter()
        .map(|c| {
            let mut o = serde_json::Map::new();
            o.insert("kind".into(), Value::String(c.kind.clone()));
            if let Some(u) = &c.url {
                o.insert("url".into(), Value::String(u.clone()));
            }
            if let Some(mi) = &c.mime {
                o.insert("mime".into(), Value::String(mi.clone()));
            }
            Value::Object(o)
        })
        .collect();
    json!({
        "id": id.as_str(),
        "name": m.plugin.name,
        "version": m.plugin.version,
        "capabilities": caps,
    })
}

/// `sy plugin enable <id>` — remove `id` from the disabled-list TOML.
/// Idempotent: succeeds even if the id wasn't in the list.
fn enable_cmd(id: &str) -> Result<()> {
    update_disabled_list(|ids| {
        ids.retain(|s| s != id);
    })
}

/// `sy plugin disable <id>` — add `id` to the disabled-list TOML.
/// Idempotent: succeeds even if the id is already present.
fn disable_cmd(id: &str) -> Result<()> {
    update_disabled_list(|ids| {
        if !ids.iter().any(|s| s == id) {
            ids.push(id.to_string());
        }
    })
}

/// Read `$SY_PLUGIN_DISABLED_TOML` (or the default XDG path), apply
/// `mutate` to the in-memory list, write back atomically (write-tmp
/// then rename).
fn update_disabled_list(mutate: impl FnOnce(&mut Vec<String>)) -> Result<()> {
    let path = disabled_toml_path().ok_or_else(|| {
        anyhow!("no path for disabled.toml (set $XDG_STATE_HOME or $SY_PLUGIN_DISABLED_TOML)")
    })?;
    let mut ids = read_disabled_list(&path);
    mutate(&mut ids);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let body = format!(
        "disabled = [{}]\n",
        ids.iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Read the existing disabled-list, returning an empty vec if the
/// file is missing or malformed (mirrors the registry's tolerant
/// load — `sy plugin enable foo` must not crash on a stale TOML).
fn read_disabled_list(path: &Path) -> Vec<String> {
    let Ok(src) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    #[derive(serde::Deserialize)]
    struct D {
        #[serde(default)]
        disabled: Vec<String>,
    }
    toml::from_str::<D>(&src)
        .map(|d| d.disabled)
        .unwrap_or_default()
}

/// Resolve the disabled-toml path the same way the registry does.
/// Duplicated rather than imported because the registry exposes only
/// the env-var name, not the resolver — Step 9 may consolidate.
fn disabled_toml_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(registry::DISABLED_TOML_ENV) {
        return Some(PathBuf::from(p));
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return Some(
            PathBuf::from(d)
                .join("sy")
                .join("plugin")
                .join("disabled.toml"),
        );
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("sy")
            .join("plugin")
            .join("disabled.toml"),
    )
}

/// SPEC §4.5 `sy plugin doctor`. Runs every check against every
/// discovered manifest; any failure flips the process exit to 8.
fn doctor_cmd(json_out: bool) -> Result<()> {
    let reg = registry::discover().context("discover plugins")?;
    let mut checks: Vec<DoctorCheck> = Vec::new();
    for id in reg.plugin_ids() {
        let Some(m) = reg.manifest(id) else {
            continue;
        };
        for c in run_checks(id, m, &reg) {
            checks.push(c);
        }
    }
    let all_green = checks.iter().all(|c| c.ok);
    if json_out {
        let doc = json!({
            "schema": SCHEMA_DOCTOR,
            "checks": checks
                .iter()
                .map(|c| {
                    json!({
                        "plugin": c.plugin,
                        "name": c.name,
                        "ok": c.ok,
                        "detail": c.detail,
                    })
                })
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string(&doc)?);
    } else {
        for c in &checks {
            let mark = if c.ok { "ok" } else { "FAIL" };
            println!("{:<6} {:<24} {}  {}", mark, c.plugin, c.name, c.detail);
        }
    }
    if !all_green {
        std::process::exit(EXIT_PLUGIN_UNHEALTHY);
    }
    Ok(())
}

/// One row of doctor output. `ok = false` flips the process exit
/// code to [`EXIT_PLUGIN_UNHEALTHY`].
#[derive(Debug, Clone)]
struct DoctorCheck {
    plugin: String,
    name: &'static str,
    ok: bool,
    detail: String,
}

/// Run every per-plugin check, returning a vec of `DoctorCheck`. The
/// list is intentionally explicit (not a trait) so SPEC §4.5 readers
/// can map check names 1:1 with this function. `reg` is passed in so
/// the routing-loopback check (`Check 3`) can ask the dispatch index
/// "does this plugin's own predicate route back to itself?" — the
/// exact lookup journey J3's hover preview path will perform.
fn run_checks(id: &PluginId, m: &Manifest, reg: &Registry) -> Vec<DoctorCheck> {
    let plugin = id.as_str().to_string();
    let mut out = Vec::new();
    // Check 1 — manifest is internally consistent (re-runs the SPEC
    // §4.1 validate path so doctor catches drift from a hot-edited
    // file the registry already accepted).
    out.push(match manifest::validate(m) {
        Ok(()) => DoctorCheck {
            plugin: plugin.clone(),
            name: "manifest.valid",
            ok: true,
            detail: "manifest passes SPEC §4.1 validation".into(),
        },
        Err(e) => DoctorCheck {
            plugin: plugin.clone(),
            name: "manifest.valid",
            ok: false,
            detail: format!("{e:#}"),
        },
    });
    // Check 2 — declared binary exists and is executable. Relative
    // paths in `[plugin.binary] exec` (the common shape for
    // `sy plugin install`-deployed plugins, which land under
    // `$XDG_DATA_HOME/sy/plugins/<id>/`) are resolved against the
    // manifest directory the registry recorded at discovery time.
    // Absolute paths pass through unchanged so existing fixtures
    // that ship absolute exec paths keep working.
    let raw_exec = PathBuf::from(&m.plugin.binary.exec);
    let bin = if raw_exec.is_absolute() {
        raw_exec
    } else if let Some(dir) = reg.manifest_dir(id) {
        dir.join(&raw_exec)
    } else {
        raw_exec
    };
    let bin_meta = std::fs::metadata(&bin);
    let bin_ok = bin_meta
        .as_ref()
        .map(|md| md.is_file() && is_executable(md))
        .unwrap_or(false);
    out.push(DoctorCheck {
        plugin: plugin.clone(),
        name: "binary.reachable",
        ok: bin_ok,
        detail: if bin_ok {
            format!("{} is executable", bin.display())
        } else {
            format!(
                "{} not found or not executable ({:?})",
                bin.display(),
                bin_meta.err().map(|e| e.kind())
            )
        },
    });
    // Check 3 — every capability the plugin declares routes back to
    // itself through the dispatch index. Catches a class of bugs the
    // hover-J3 path would otherwise hit silently: a manifest whose
    // url/mime predicate compiles but matches nothing, so
    // `Registry::select_for` returns `None` for the file the user
    // hovers over.
    for cap in &m.capabilities {
        let kind = match CapKind::from_manifest_kind(&cap.kind) {
            Some(k) => k,
            None => continue,
        };
        let probe_mime = cap.mime.as_deref().unwrap_or("application/octet-stream");
        let probe_url = cap.url.as_deref().unwrap_or("__doctor.probe");
        let resolved = reg.select_for(kind, probe_mime, probe_url);
        let routed_to_self = resolved == Some(id);
        out.push(DoctorCheck {
            plugin: plugin.clone(),
            name: "capability.routes",
            ok: routed_to_self,
            detail: if routed_to_self {
                format!(
                    "({}, mime={}, url={}) routes to {}",
                    cap.kind,
                    probe_mime,
                    probe_url,
                    id.as_str()
                )
            } else {
                format!(
                    "({}, mime={}, url={}) routed to {:?} instead of self",
                    cap.kind, probe_mime, probe_url, resolved,
                )
            },
        });
    }
    out
}

/// `true` if the mode bits include at least one executable bit.
fn is_executable(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

/// `sy plugin exec <id> <method> --params '<json>'` — spawn the
/// plugin under the SPEC §4.3 sandbox envelope, run the SPEC §4.2.3
/// handshake, send one request, capture the result, exit. Tears down
/// the child on the way out via the supervisor's graceful shutdown.
fn exec_cmd(id: &str, method: &str, params: Option<&str>) -> Result<()> {
    let params_value: Value = match params {
        None => Value::Null,
        Some(s) => serde_json::from_str(s)
            .with_context(|| format!("--params must be a JSON value, got: {s:?}"))?,
    };
    let reg = registry::discover().context("discover plugins")?;
    let plugin_id = PluginId(id.to_string());
    let manifest = reg
        .manifest(&plugin_id)
        .cloned()
        .ok_or_else(|| anyhow!("no plugin with id {id:?} found in any discovery root"))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for plugin exec")?;
    rt.block_on(async move {
        let workdir = sandbox::runtime_dir_for(id);
        std::fs::create_dir_all(&workdir)
            .with_context(|| format!("mkdir workdir {}", workdir.display()))?;
        let (host_ctx, _notify_rx) = host_fns::ctx_for(workdir.clone(), Value::Null);
        let mut opts = SpawnOpts::new(workdir);
        opts.host_ctx = Some(host_ctx);
        // `request_timeout` is intentionally pinned wide for the
        // one-shot CLI path — `sy plugin exec` is interactive and
        // a slow plugin should surface its body to the operator,
        // not a Timeout error. Production supervisor callers (Step
        // 13+ file-manager daemon) keep the SPEC default.
        opts.request_timeout = std::time::Duration::from_secs(30);
        let mut sup = proc::spawn(manifest, opts)
            .await
            .map_err(|e| anyhow!("spawn {id}: {e}"))?;
        // `spawn` returns once the handshake completes, but the
        // public contract is `health() == Ready`; assert that here
        // so the wait-for-ready helper stays exercised by the bin
        // (Step 13+ daemon will rely on the same predicate).
        sup.wait_ready()
            .await
            .map_err(|e| anyhow!("plugin {id} did not reach Ready: {e}"))?;
        // Snapshot the supervisor's post-handshake state + caps for
        // operator diagnostics — both surface in `tracing::debug!` so
        // `RUST_LOG=debug sy plugin exec ...` shows what the plugin
        // advertised at `initialize`.
        let post_handshake_state = sup.health();
        let advertised = sup.caps().map(|c| c.plugin_capabilities.len()).unwrap_or(0);
        tracing::debug!(
            target = "sy::plugin::cli",
            plugin_id = %id,
            state = ?post_handshake_state,
            advertised_caps = advertised,
            "plugin ready for exec"
        );
        let result = sup
            .request(method, params_value)
            .await
            .map_err(|e| format_rpc_error(method, e));
        // Always attempt a graceful shutdown so the child reaps
        // cleanly even when the request errored out.
        let _ = sup.shutdown().await;
        let value = result?;
        println!("{}", serde_json::to_string(&value)?);
        anyhow::Ok(())
    })
}

/// Render an [`RpcError`] from `sy plugin exec` into a human-readable
/// `anyhow::Error`. The `Peer { code, message, data }` arm spells the
/// JSON-RPC code + the optional `data` payload so the operator can
/// see exactly what the plugin emitted — this is the only bin
/// consumer of `RpcError::Peer.data` today.
fn format_rpc_error(method: &str, e: RpcError) -> anyhow::Error {
    match e {
        RpcError::Peer {
            code,
            message,
            data,
        } => anyhow!("plugin {method} returned error code={code} msg={message} data={data}"),
        other => anyhow!("request {method}: {other}"),
    }
}

/// `sy plugin cat-manifest <id>` — pretty-print the discovered
/// manifest body as TOML on stdout. Round-trips through `toml::ser`
/// so the user sees a stable, normalised representation.
fn cat_manifest_cmd(id: &str) -> Result<()> {
    let reg = registry::discover().context("discover plugins")?;
    let plugin_id = PluginId(id.to_string());
    let m = reg
        .manifest(&plugin_id)
        .ok_or_else(|| anyhow!("no plugin with id {id:?} found"))?;
    let body = toml::to_string_pretty(m).with_context(|| format!("serialise manifest for {id}"))?;
    println!("{body}");
    Ok(())
}

/// `sy plugin validate <path>` — read + parse + validate a manifest
/// file. Exits [`EXIT_USAGE`] (2) on any failure with a human-readable
/// error on stderr; exits 0 on success with a "ok" line on stdout.
fn validate_cmd(path: &Path) -> Result<()> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: read {}: {e}", path.display());
            std::process::exit(EXIT_USAGE);
        }
    };
    match manifest::load(&src) {
        Ok(m) => {
            println!(
                "ok: {} ({}@{})",
                path.display(),
                m.plugin.id,
                m.plugin.version
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("error: validate {}: {:#}", path.display(), e);
            std::process::exit(EXIT_USAGE);
        }
    }
}

/// `sy plugin reload` — re-scan the discovery roots. Today this is
/// a no-op (every CLI invocation already re-scans); Step 13+ will
/// signal the file-manager daemon to drop + rebuild its registry.
fn reload_cmd() -> Result<()> {
    let reg = registry::discover().context("re-scan plugins")?;
    let count = reg.plugin_ids().count();
    println!("reloaded {count} plugin(s)");
    Ok(())
}

/// `sy plugin install <source>` — see [`PluginCmd::Install`]. Parses
/// the source string, builds the install options against the
/// installed lane (XDG data home or `$SY_PLUGIN_INSTALL_DIR` override
/// for tests), runs the install pipeline, and exits with the
/// SPEC-§4.5 code corresponding to any [`install::InstallError`].
fn install_cmd(source: &str, unsigned: bool, rev: Option<&str>) -> Result<()> {
    let parsed = parse_install_source(source, rev)?;
    let dest_root = resolve_install_root()
        .ok_or_else(|| anyhow!("no XDG_DATA_HOME or HOME; set $SY_PLUGIN_INSTALL_DIR"))?;
    let publishers_dir = resolve_publishers_dir();
    let mut opts = InstallOpts::new(dest_root);
    opts.unsigned = unsigned;
    opts.publishers_dir = publishers_dir;
    match install::install(parsed, opts) {
        Ok(installed) => {
            println!("installed {} -> {}", installed.id, installed.dir.display());
            Ok(())
        }
        Err(e) => {
            let code = match e {
                install::InstallError::SignatureInvalid(_) => EXIT_SIGNATURE_INVALID,
                install::InstallError::ManifestInvalid(_) => EXIT_MANIFEST_INVALID,
                install::InstallError::Io(_) => 1,
            };
            eprintln!("error: install: {e}");
            std::process::exit(code);
        }
    }
}

/// Parse the user-typed source. `git+<url>` → [`InstallSource::Git`];
/// anything else is treated as a filesystem path. The `--rev` flag
/// is forwarded only for the git form; we warn (do not error) when a
/// user passes `--rev` with a path source so the future move to a
/// version-aware path install isn't an API break.
fn parse_install_source(source: &str, rev: Option<&str>) -> Result<InstallSource> {
    if let Some(url) = source.strip_prefix("git+") {
        return Ok(InstallSource::Git {
            url: url.to_string(),
            rev: rev.map(|s| s.to_string()),
        });
    }
    if rev.is_some() {
        eprintln!("warn: --rev is ignored for non-git sources");
    }
    Ok(InstallSource::Path(PathBuf::from(source)))
}

/// `$SY_PLUGIN_INSTALL_DIR` overrides the install destination root;
/// otherwise we use `$XDG_DATA_HOME/sy/plugins/` with the
/// freedesktop fallback to `$HOME/.local/share/sy/plugins/`.
///
/// The override is the same env-shaped CLIG ratchet the registry uses
/// for its discovery root (`SY_PLUGIN_DIR`) — keeps integration tests
/// hermetic without exporting `XDG_DATA_HOME` from the test process.
fn resolve_install_root() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SY_PLUGIN_INSTALL_DIR") {
        return Some(PathBuf::from(p));
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(d).join("sy").join("plugins"));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("sy")
            .join("plugins"),
    )
}

/// Resolve the publisher-pubkey directory. Honours
/// `$SY_PLUGIN_PUBLISHERS_DIR` for test hermeticity, then falls back
/// to the productivised in-repo lane `configs/sy/plugin-publishers/`.
fn resolve_publishers_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SY_PLUGIN_PUBLISHERS_DIR") {
        return PathBuf::from(p);
    }
    PathBuf::from("configs/sy/plugin-publishers")
}

/// `sy plugin uninstall <id>` — `rm -rf` the plugin's install dir.
/// Idempotent: returns 0 even when the dir doesn't exist (the user
/// may be cleaning up a half-failed install or running on a host that
/// never had the plugin).
fn uninstall_cmd(id: &str) -> Result<()> {
    let root = resolve_install_root()
        .ok_or_else(|| anyhow!("no XDG_DATA_HOME or HOME; set $SY_PLUGIN_INSTALL_DIR"))?;
    let dir = root.join(id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        println!("uninstalled {id} (removed {})", dir.display());
    } else {
        println!(
            "uninstalled {id} (no-op; not installed at {})",
            dir.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Unit tests for the helpers that don't need to spawn a child.
    //! End-to-end behaviour lives in `tests/sy_plugin_cli.rs` (drives
    //! the real binary via `CARGO_BIN_EXE_sy`).
    use super::*;

    /// `is_executable` reads the unix mode bits — both "any execute
    /// bit" cases (user/group/other) flip the result to `true`.
    #[test]
    fn is_executable_reads_mode_bits() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tmp");
        let p = tmp.path().join("script");
        std::fs::write(&p, "").expect("write");
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&p, perms).unwrap();
        assert!(!is_executable(&std::fs::metadata(&p).unwrap()));
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
        assert!(is_executable(&std::fs::metadata(&p).unwrap()));
    }

    /// The list schema renders capability rows preserving both `url`
    /// and `mime` predicates so MCP agents don't need to re-parse the
    /// raw TOML to route preview requests.
    #[test]
    fn manifest_to_json_includes_url_and_mime() {
        const SRC: &str = r#"
api = "1"

[plugin]
id = "foo"
name = "Foo"
version = "1.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "/bin/true"

[[capability]]
kind = "previewer"
url = "*.md"
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
spawn_timeout_ms = 500
shutdown_timeout_ms = 500
"#;
        let m = manifest::load(SRC).expect("parse");
        let v = manifest_to_json(&PluginId("foo".into()), &m);
        let caps = v["capabilities"].as_array().expect("capabilities");
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0]["url"], "*.md");
        assert_eq!(caps[1]["mime"], "text/markdown");
        assert_eq!(v["id"], "foo");
        assert_eq!(v["version"], "1.0");
    }
}
