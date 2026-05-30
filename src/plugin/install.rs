//! `sy plugin install` — local path + git URL fetch, minisign signature
//! verify, atomic stage→swap into `$XDG_DATA_HOME/sy/plugins/<id>/`.
//! Step 9 of the [`sy-file-manager` roadmap][roadmap]; closes the
//! distribution row from [plugin SPEC §3.2][spec-decisions] and the
//! install flow under [§4.5][spec-cli].
//!
//! ## Atomicity model
//!
//! Every install stages under `<dest_root>/.staging-<id>-<rand>/`,
//! resolves + parses the manifest there, verifies the minisign
//! signature (unless `--unsigned`), and only at the very end
//! `rename(2)`s the staging dir into place at `<dest_root>/<id>/`.
//! `rename` is the commit point — a crash before that step leaves the
//! staging dir behind (best-effort cleaned up by [`InstallScope`]
//! drop) but never a partially-populated `<id>/`.
//!
//! Reinstall uses the **swap-then-cleanup** pattern: the existing
//! `<id>/` is first renamed to `<id>.old-<ts>/`; if the swap-in
//! `rename` succeeds, the `.old-*` is unlinked. If the swap-in
//! `rename` fails (e.g. the destination dir became unwritable
//! mid-flight), the old dir is renamed back. This survives mid-
//! flight crashes — at worst the user has an `<id>.old-*` sitting
//! next to a clean `<id>/` which the next install or a `sy plugin
//! reload` reconciles.
//!
//! ## Signature canonical form
//!
//! SPEC §4.1 calls for a minisign signature "over the binary +
//! manifest". The canonical signed payload, locked here so future
//! re-signers stay byte-compatible:
//!
//! 1. The plugin binary bytes (path resolved against the manifest
//!    dir: `[plugin.binary] exec`).
//! 2. A single NUL byte (`0x00`) — domain separator so a binary
//!    that ends in valid TOML can't collide with a manifest-only
//!    signature.
//! 3. The `plugin.toml` source bytes **with the
//!    `[plugin.signature]` block stripped** (otherwise the
//!    signature would sign itself).
//!
//! The stripping is a textual sed-shaped pass: lines from
//! `[plugin.signature]` up to (exclusive) the next `[` section
//! header — or EOF — are dropped. Inline TOML strings that happen
//! to contain `[plugin.signature]` are not a concern because they
//! can't start a logical line.
//!
//! [roadmap]: ../../../specs/roadmaps/sy-file-manager/ROADMAP.md
//! [spec-decisions]: ../../../specs/research/sy-file-manager-plugins/SPEC.md#32-key-decisions
//! [spec-cli]: ../../../specs/research/sy-file-manager-plugins/SPEC.md#45-cli--mcp-surface
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use minisign_verify::{PublicKey, Signature};

use crate::plugin::manifest::{self, Manifest};

/// Env var name for the per-spawn signature bypass. Set
/// `SY_PLUGIN_NO_SIGNATURE=1` to short-circuit verification on every
/// spawn; each spawn emits one `tracing::warn!` line per SPEC §4.5.
/// Wired into the supervisor by [`crate::plugin::proc::spawn`].
pub const NO_SIGNATURE_ENV: &str = "SY_PLUGIN_NO_SIGNATURE";

/// Domain-separator byte placed between the binary and the manifest
/// in the canonical signed payload (see module docs).
const SIGNATURE_SEP_BYTE: u8 = 0x00;

/// Where `install` looks for `<name>.pub` when the manifest's
/// `[plugin.signature] pubkey` is a publisher name rather than an
/// inline base64 key.
const PUBLISHERS_REL_DIR: &str = "configs/sy/plugin-publishers";

/// The git binary the path-form of `sy plugin install` shells out to
/// for `<git+url>` sources. We never use a git library — `git` is
/// already on every Fedora host and the only operation we need is
/// a single `git clone --depth 1` into a tempdir.
const GIT_BIN: &str = "/usr/bin/git";

/// Source the user passed to `sy plugin install`. The clap layer
/// parses `<source>` strings prefixed with `git+` as [`Self::Git`];
/// anything else is a [`Self::Path`].
#[derive(Debug, Clone)]
pub enum InstallSource {
    /// Local directory containing `plugin.toml` + `bin/...`.
    Path(PathBuf),
    /// Git URL (any transport git accepts: `https://`, `file://`,
    /// `git@…`). `rev` is an optional ref / commit to check out after
    /// the clone.
    Git { url: String, rev: Option<String> },
}

/// Optional knobs surfaced through the CLI.
#[derive(Debug, Clone)]
pub struct InstallOpts {
    /// Bypass signature verification for local / development plugins
    /// that don't ship a `[plugin.signature]` block. SPEC §4.5
    /// `--unsigned`.
    pub unsigned: bool,
    /// Override the install root. Defaults to
    /// `$XDG_DATA_HOME/sy/plugins/` (or `~/.local/share/sy/plugins/`).
    /// Integration tests point this at a tempdir.
    pub dest_root: PathBuf,
    /// Override the publisher-pubkey directory. Defaults to
    /// `<workspace>/configs/sy/plugin-publishers/`. Tests point this
    /// at a tempdir holding a freshly-minted keypair.
    pub publishers_dir: PathBuf,
}

impl InstallOpts {
    /// Construct an options block pointing at `dest_root`. The
    /// publisher directory defaults to the productivised lane —
    /// callers (tests / mocked hosts) override via `with_publishers_dir`.
    pub fn new(dest_root: PathBuf) -> Self {
        Self {
            unsigned: false,
            dest_root,
            publishers_dir: PathBuf::from(PUBLISHERS_REL_DIR),
        }
    }
}

/// Outcome of a successful install. `id` mirrors `Manifest::plugin.id`;
/// `dir` is the final landing path under `dest_root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub id: String,
    pub dir: PathBuf,
}

/// Error kinds surfaced from the install flow. SPEC §4.5 maps each
/// to a stable CLI exit code; the [`install_error_exit_code`] helper
/// is the single chokepoint translating these to integers so the
/// table in `cli.rs` and the table in SPEC §4.5 can't drift.
#[derive(Debug)]
pub enum InstallError {
    /// Source path / clone failure, missing files, malformed
    /// manifest, I/O failures during staging. Generic recoverable
    /// errors map to SPEC §4.5 exit 1.
    Io(String),
    /// Manifest TOML failed parse or validation. Maps to SPEC §4.5
    /// exit 6.
    ManifestInvalid(String),
    /// Signature missing when required (no `--unsigned`), invalid
    /// minisign encoding, or signature did not verify against the
    /// payload. Maps to SPEC §4.5 exit 7.
    SignatureInvalid(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Io(m) => write!(f, "io: {m}"),
            InstallError::ManifestInvalid(m) => write!(f, "manifest invalid: {m}"),
            InstallError::SignatureInvalid(m) => write!(f, "signature invalid: {m}"),
        }
    }
}

impl std::error::Error for InstallError {}

/// Map an [`InstallError`] to its SPEC §4.5 exit code. Lives here
/// (rather than in `cli.rs`) so the mapping is co-located with the
/// error variants and unit tests in this module can pin SPEC §4.5
/// against the type — the bin's `install_cmd` inlines the same
/// match so the compiler catches drift if a new variant is added.
#[cfg(test)]
fn install_error_exit_code(e: &InstallError) -> i32 {
    match e {
        InstallError::Io(_) => 1,
        InstallError::ManifestInvalid(_) => 6,
        InstallError::SignatureInvalid(_) => 7,
    }
}

/// Stage → verify → atomic rename. Returns the final
/// [`InstalledPlugin`] once the rename commits.
pub fn install(source: InstallSource, opts: InstallOpts) -> Result<InstalledPlugin, InstallError> {
    std::fs::create_dir_all(&opts.dest_root).map_err(|e| {
        InstallError::Io(format!("mkdir dest_root {}: {e}", opts.dest_root.display()))
    })?;
    // 1. Materialise the source into a fetch dir we own. The leading
    //    dot keeps the dir out of `Registry::scan_root` even if a
    //    crash leaves it behind; the fetch scope deletes on drop.
    let fetch_dir = opts.dest_root.join(format!(".fetch-{}", staging_token()));
    std::fs::create_dir_all(&fetch_dir)
        .map_err(|e| InstallError::Io(format!("mkdir fetch {}: {e}", fetch_dir.display())))?;
    // Held until end-of-fn so the fetch dir is unlinked on any
    // return path. Never committed — it's source-side scratch.
    let _fetch_scope = InstallScope::new(fetch_dir.clone());
    materialise_source(&source, &fetch_dir)?;
    let src_root = locate_manifest_dir(&fetch_dir)?;

    // 2. Read + validate the manifest so we know the id before staging.
    let manifest_src = std::fs::read_to_string(src_root.join("plugin.toml")).map_err(|e| {
        InstallError::Io(format!("read plugin.toml in {}: {e}", src_root.display()))
    })?;
    let manifest = manifest::load(&manifest_src)
        .map_err(|e| InstallError::ManifestInvalid(format!("{e:#}")))?;
    let plugin_id = manifest.plugin.id.clone();

    // 3. Stage into `<dest_root>/.staging-<id>-<rand>/`.
    let staging = staging_path_for(&opts.dest_root, &plugin_id);
    let scope = InstallScope::new(staging.clone());
    copy_tree(&src_root, &staging)
        .map_err(|e| InstallError::Io(format!("copy into staging {}: {e}", staging.display())))?;

    // 4. Verify the signature unless the user opted out.
    if !opts.unsigned {
        verify_signature(&staging, &manifest, &opts.publishers_dir)?;
    } else if manifest.signature.is_some() {
        tracing::warn!(
            target = "sy::plugin::install",
            plugin_id = %plugin_id,
            "--unsigned: manifest carries [plugin.signature] but verification skipped"
        );
    } else {
        tracing::warn!(
            target = "sy::plugin::install",
            plugin_id = %plugin_id,
            "--unsigned: installing without signature verification"
        );
    }

    // 5. Commit: swap into <dest_root>/<id>/.
    let final_dir = opts.dest_root.join(&plugin_id);
    swap_into_place(&staging, &final_dir)
        .map_err(|e| InstallError::Io(format!("swap-into-place {}: {e}", final_dir.display())))?;
    scope.commit();
    Ok(InstalledPlugin {
        id: plugin_id,
        dir: final_dir,
    })
}

/// Drop guard that unlinks the staging dir if [`Self::commit`] was
/// never called. Survives panics through `Drop`; the install path
/// commits explicitly once the rename succeeds.
struct InstallScope {
    path: Option<PathBuf>,
}

impl InstallScope {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }
    /// Tell the guard the install succeeded — the staging dir has
    /// already been renamed into place and must NOT be deleted.
    fn commit(mut self) {
        self.path = None;
    }
}

impl Drop for InstallScope {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            // Best-effort: a failed rmtree still leaves a dir prefixed
            // with `.staging-` that the next install or a manual sweep
            // can clean up.
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// Construct the staging path: `<dest_root>/.staging-<id>-<rand>/`.
/// The leading dot keeps the directory out of `Registry::scan_root`,
/// which only enumerates `<root>/*/plugin.toml`.
fn staging_path_for(dest_root: &Path, id: &str) -> PathBuf {
    let rand = staging_token();
    dest_root.join(format!(".staging-{id}-{rand}"))
}

/// Generate a short random token for the staging dir name. Uses
/// `ulid::Ulid::new()` because it's already a workspace dep
/// (`sy-ipc` request ids).
fn staging_token() -> String {
    let id = ulid::Ulid::new();
    id.to_string()
}

/// Materialise an [`InstallSource`] into `dst`. For [`Path`] we copy
/// the tree; for [`Git`] we shell out to `/usr/bin/git clone --depth
/// 1 [-b rev]`.
fn materialise_source(source: &InstallSource, dst: &Path) -> Result<(), InstallError> {
    match source {
        InstallSource::Path(p) => {
            let root = if p.join("plugin.toml").is_file() {
                p.clone()
            } else {
                return Err(InstallError::Io(format!(
                    "no plugin.toml at {} (need <path>/plugin.toml)",
                    p.display()
                )));
            };
            copy_tree(&root, dst)
                .map_err(|e| InstallError::Io(format!("copy {}: {e}", root.display())))
        }
        InstallSource::Git { url, rev } => clone_git(url, rev.as_deref(), dst),
    }
}

/// Shell out to `git clone --depth 1` (or full clone + checkout when a
/// rev is provided, since `--depth 1 -b <commit>` doesn't work for raw
/// SHAs). The clone target is `dst`; the dir must already exist (it's
/// the tempdir we own) so we clone into a subdir and locate the
/// manifest dir downstream via [`locate_manifest_dir`].
fn clone_git(url: &str, rev: Option<&str>, dst: &Path) -> Result<(), InstallError> {
    let target = dst.join("repo");
    let mut cmd = StdCommand::new(GIT_BIN);
    cmd.arg("clone");
    if rev.is_none() {
        cmd.args(["--depth", "1"]);
    }
    cmd.arg(url).arg(&target);
    let out = cmd
        .output()
        .map_err(|e| InstallError::Io(format!("spawn {GIT_BIN} clone: {e}")))?;
    if !out.status.success() {
        return Err(InstallError::Io(format!(
            "git clone {url} failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    if let Some(r) = rev {
        let out = StdCommand::new(GIT_BIN)
            .arg("-C")
            .arg(&target)
            .args(["checkout", r])
            .output()
            .map_err(|e| InstallError::Io(format!("spawn git checkout: {e}")))?;
        if !out.status.success() {
            return Err(InstallError::Io(format!(
                "git checkout {r} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }
    Ok(())
}

/// Find the `plugin.toml` inside a freshly-materialised tree. We
/// allow either `<root>/plugin.toml` directly or one level of nesting
/// (`<root>/<single-subdir>/plugin.toml`), the latter being the
/// shape `git clone` lands. Anything deeper is rejected so a hostile
/// archive can't smuggle a manifest 8 directories down.
fn locate_manifest_dir(root: &Path) -> Result<PathBuf, InstallError> {
    if root.join("plugin.toml").is_file() {
        return Ok(root.to_path_buf());
    }
    let entries = std::fs::read_dir(root)
        .map_err(|e| InstallError::Io(format!("read_dir {}: {e}", root.display())))?;
    for ent in entries.flatten() {
        let p = ent.path();
        if p.is_dir() && p.join("plugin.toml").is_file() {
            return Ok(p);
        }
    }
    Err(InstallError::Io(format!(
        "no plugin.toml found under {} (looked at root + immediate subdirs)",
        root.display()
    )))
}

/// Recursive copy with `std::fs`. `tokio::fs` would buy us nothing
/// (install is one-shot) and shelling out to `cp -a` is one fewer
/// dependency for the same effect.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let from = ent.path();
        let to = dst.join(ent.file_name());
        let ft = ent.file_type()?;
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_symlink() {
            // Resolve through the link (manifests point at real
            // binaries; preserving the link itself would leave a
            // dangling reference once the source dir is unlinked).
            std::fs::copy(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
            // Preserve mode bits so the binary stays executable.
            let perms = std::fs::metadata(&from)?.permissions();
            std::fs::set_permissions(&to, perms)?;
        }
    }
    Ok(())
}

/// Atomic swap: rename `staging` → `final_dir`. If `final_dir`
/// already exists, rename it to `<final_dir>.old-<ts>` first; if the
/// swap-in rename fails after the old-rename, rename it back so the
/// user is never left with a missing plugin.
fn swap_into_place(staging: &Path, final_dir: &Path) -> std::io::Result<()> {
    let parent = final_dir.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("final_dir {} has no parent", final_dir.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let prior: Option<PathBuf> = if final_dir.exists() {
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%3f").to_string();
        let old = final_dir.with_file_name(format!(
            "{}.old-{ts}",
            final_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
        ));
        std::fs::rename(final_dir, &old)?;
        Some(old)
    } else {
        None
    };
    match std::fs::rename(staging, final_dir) {
        Ok(()) => {
            if let Some(old) = prior {
                let _ = std::fs::remove_dir_all(&old);
            }
            Ok(())
        }
        Err(e) => {
            // Best-effort rollback so the user isn't left without
            // their plugin.
            if let Some(old) = prior {
                let _ = std::fs::rename(&old, final_dir);
            }
            Err(e)
        }
    }
}

/// SPEC §4.1 signature check. Reads `[plugin.signature]`, resolves
/// the pubkey, computes the canonical payload, verifies via
/// `minisign-verify`. Missing signature when verification is required
/// returns [`InstallError::SignatureInvalid`].
pub fn verify_signature(
    manifest_dir: &Path,
    manifest: &Manifest,
    publishers_dir: &Path,
) -> Result<(), InstallError> {
    let sig_block = manifest.signature.as_ref().ok_or_else(|| {
        InstallError::SignatureInvalid(
            "manifest has no [plugin.signature]; pass --unsigned to install for local development"
                .into(),
        )
    })?;
    let pubkey = resolve_pubkey(&sig_block.pubkey, publishers_dir)?;
    let sig_text = resolve_sig_text(&sig_block.sig, manifest_dir)?;
    let sig = Signature::decode(&sig_text)
        .map_err(|e| InstallError::SignatureInvalid(format!("decode signature: {e}")))?;
    let payload = canonical_signed_payload(manifest_dir, manifest)?;
    pubkey
        .verify(&payload, &sig, /* allow_legacy */ false)
        .map_err(|e| InstallError::SignatureInvalid(format!("verify: {e}")))?;
    Ok(())
}

/// Length of a minisign public key encoded as base64. The on-the-
/// wire shape is `<2-byte sig algo> || <8-byte key id> || <32-byte
/// ed25519 pk> = 42 bytes`, which base64-encodes to 56 characters
/// (no padding). The leading two bytes are always `Ed` (signature
/// algorithm = Ed25519) which after base64 produces the canonical
/// `RW` prefix that minisign tooling promises in every public key.
const MINISIGN_PUBKEY_B64_LEN: usize = 56;

/// Resolve the pubkey field. Forms accepted:
///
/// * Bare minisign base64 (`RW...`, exactly 56 chars on a single
///   line) — parsed inline.
/// * Multi-line minisign public-key text (with `untrusted comment:`
///   header) — the first non-comment line is parsed.
/// * Plain `<name>` (no embedded whitespace or path separator) —
///   read `publishers_dir/<name>.pub`.
fn resolve_pubkey(field: &str, publishers_dir: &Path) -> Result<PublicKey, InstallError> {
    let trimmed = field.trim();
    if is_inline_minisign_pubkey(trimmed) {
        return PublicKey::from_base64(trimmed)
            .map_err(|e| InstallError::SignatureInvalid(format!("decode inline pubkey: {e}")));
    }
    if trimmed.starts_with("untrusted comment") {
        let line = trimmed
            .lines()
            .find(|l| !l.starts_with("untrusted comment"))
            .ok_or_else(|| {
                InstallError::SignatureInvalid(
                    "pubkey block has no base64 line under the untrusted-comment header".into(),
                )
            })?;
        return PublicKey::from_base64(line.trim())
            .map_err(|e| InstallError::SignatureInvalid(format!("decode pubkey block: {e}")));
    }
    // Treat as a publisher name.
    let path = publishers_dir.join(format!("{trimmed}.pub"));
    let body = std::fs::read_to_string(&path).map_err(|e| {
        InstallError::SignatureInvalid(format!("read publisher pubkey {}: {e}", path.display()))
    })?;
    let line = body
        .lines()
        .find(|l| !l.trim().is_empty() && !l.starts_with("untrusted comment"))
        .ok_or_else(|| {
            InstallError::SignatureInvalid(format!(
                "publisher pubkey {} has no base64 line",
                path.display()
            ))
        })?;
    PublicKey::from_base64(line.trim()).map_err(|e| {
        InstallError::SignatureInvalid(format!("decode publisher pubkey {}: {e}", path.display()))
    })
}

/// `true` when `s` is a single-line, fixed-length minisign public-key
/// base64 string. The check is exact (56 chars, starts with `RW`, no
/// internal whitespace) so a publisher name like `sy-plugin-md` can't
/// be mis-identified as inline base64.
fn is_inline_minisign_pubkey(s: &str) -> bool {
    s.starts_with("RW") && s.len() == MINISIGN_PUBKEY_B64_LEN && !s.contains(char::is_whitespace)
}

/// Resolve the signature payload. Forms accepted:
///
/// * Inline minisign text (the full `untrusted comment:\n<sig>\n
///   trusted comment:\n<global>\n` block) — passed straight to
///   `Signature::decode`.
/// * `@<relpath>` — read `manifest_dir/<relpath>` (the on-disk
///   `.minisig` file the publisher shipped alongside the binary).
fn resolve_sig_text(field: &str, manifest_dir: &Path) -> Result<String, InstallError> {
    let trimmed = field.trim();
    if let Some(rel) = trimmed.strip_prefix('@') {
        let p = manifest_dir.join(rel);
        return std::fs::read_to_string(&p).map_err(|e| {
            InstallError::SignatureInvalid(format!("read signature file {}: {e}", p.display()))
        });
    }
    Ok(trimmed.to_string())
}

/// Build the canonical signed payload: binary bytes ‖ 0x00 ‖ manifest
/// bytes (with `[plugin.signature]` block stripped). See module docs.
fn canonical_signed_payload(
    manifest_dir: &Path,
    manifest: &Manifest,
) -> Result<Vec<u8>, InstallError> {
    let bin_rel = manifest.plugin.binary.exec.as_str();
    let bin_path = if Path::new(bin_rel).is_absolute() {
        PathBuf::from(bin_rel)
    } else {
        manifest_dir.join(bin_rel)
    };
    let bin_bytes = std::fs::read(&bin_path).map_err(|e| {
        InstallError::SignatureInvalid(format!(
            "read binary at {} for signature payload: {e}",
            bin_path.display()
        ))
    })?;
    let manifest_src = std::fs::read_to_string(manifest_dir.join("plugin.toml"))
        .map_err(|e| InstallError::SignatureInvalid(format!("re-read manifest: {e}")))?;
    let stripped = strip_signature_block(&manifest_src);
    let mut out = Vec::with_capacity(bin_bytes.len() + 1 + stripped.len());
    out.extend_from_slice(&bin_bytes);
    out.push(SIGNATURE_SEP_BYTE);
    out.extend_from_slice(stripped.as_bytes());
    Ok(out)
}

/// Remove the `[plugin.signature]` (or `[plugin.signature.*]`) block
/// from a TOML source: every line from the section header up to (but
/// excluding) the next section header at column 0 — or EOF — is
/// dropped. Keeps the rest of the file byte-for-byte stable.
pub fn strip_signature_block(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut skipping = false;
    for raw in src.split_inclusive('\n') {
        let line_no_ws = raw.trim_start();
        if line_no_ws.starts_with('[') {
            // Strip whitespace inside the header so `[ plugin.signature ]`
            // is treated the same as `[plugin.signature]`.
            let header = line_no_ws.trim();
            if header.starts_with("[plugin.signature]") || header.starts_with("[plugin.signature.")
            {
                skipping = true;
                continue;
            }
            skipping = false;
        }
        if !skipping {
            out.push_str(raw);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure helpers — the install end-to-end
    //! flow is exercised by `tests/sy_plugin_install.rs` (which also
    //! drives the real `sy` bin via `CARGO_BIN_EXE_sy`).
    use super::*;

    #[test]
    fn strip_signature_drops_block_keeps_rest() {
        let src = "api = \"1\"\n\n[plugin]\nid = \"x\"\n\n[plugin.signature]\nsig = \"AAA\"\npubkey = \"BBB\"\n\n[needs]\nfs_read = []\n";
        let stripped = strip_signature_block(src);
        assert!(
            !stripped.contains("[plugin.signature]"),
            "block must go: {stripped}"
        );
        assert!(
            !stripped.contains("sig = \"AAA\""),
            "sig line must go: {stripped}"
        );
        assert!(
            stripped.contains("[plugin]\nid = \"x\""),
            "[plugin] must stay"
        );
        assert!(
            stripped.contains("[needs]\nfs_read = []"),
            "[needs] must stay"
        );
    }

    #[test]
    fn strip_signature_idempotent_when_block_absent() {
        let src = "api = \"1\"\n[plugin]\nid = \"x\"\n[needs]\nfs_read = []\n";
        assert_eq!(strip_signature_block(src), src);
    }

    #[test]
    fn install_error_exit_codes_match_spec_table() {
        // SPEC §4.5: 1 generic, 6 manifest invalid, 7 signature invalid.
        assert_eq!(install_error_exit_code(&InstallError::Io("x".into())), 1);
        assert_eq!(
            install_error_exit_code(&InstallError::ManifestInvalid("x".into())),
            6
        );
        assert_eq!(
            install_error_exit_code(&InstallError::SignatureInvalid("x".into())),
            7
        );
    }

    #[test]
    fn staging_path_has_dot_prefix_and_id() {
        let dest = PathBuf::from("/tmp/sy-plugins");
        let p = staging_path_for(&dest, "sy-plugin-md");
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(".staging-sy-plugin-md-"), "got {name}");
        assert_eq!(p.parent().unwrap(), dest);
    }
}
