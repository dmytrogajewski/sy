//! Policy resolver — answers "may tool `T` with `argv` run?" without
//! spawning anything yet. Enforcement (Landlock, seccomp, scope)
//! lands in Step 3; this module is the read-only decision oracle.
//!
//! Decision rules (Step 1, no `ConsentStore` yet):
//!
//! - Empty `exec_allowlist` means **deny everything by construction**.
//!   The `strict` profile leans on this so an operator who forgets
//!   to add a per-tool overlay never gets a surprise allow.
//! - A non-empty allowlist matches when *some* `ExecAllow` entry's
//!   `bin` matches the requested tool (either an exact path or a
//!   `globset` pattern such as `*`) **and** *some* glob in that
//!   entry's `argv` matches the actual argv vector joined by a single
//!   space. The roadmap test `normal_allows_rg` asserts that
//!   `argv = ["*"]` accepts an argv of `["foo"]`.
//! - On match the `require_consent` field decides:
//!   * `Never` → `Allow`,
//!   * `OncePerSession` → `Allow` (Step 6's `ConsentStore` will
//!     downgrade unrecorded calls to `ConsentRequired`),
//!   * `EveryCall` → `ConsentRequired { reason }` (consent prompt
//!     fires every time — strict semantics).
//! - On miss (allowlist non-empty, tool/argv not matched) we return
//!   `ConsentRequired { reason }` so the operator can grant the call
//!   ad-hoc via Step 6's `sy approve <token>`.
//!
//! Per-tool overlays under `configs/policy/tools/<tool>.toml` replace
//! the profile *for that tool's decision* — the overlay is itself a
//! full `Profile` and is consulted first when the tool basename
//! matches. Future enhancement: field-level merge.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use globset::{Glob, GlobBuilder};
use sha2::{Digest, Sha256};

use crate::agt::policy::schema::{ConsentMode, ExecAllow, Profile};

/// Three-way answer from [`Resolver::decide`]. The `ConsentRequired`
/// variant carries a human-readable `reason` so the audit log and
/// the future `sy approve <token>` flow can surface *why* a tool
/// call paused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    ConsentRequired { reason: String },
}

/// Resolved policy ready to answer `decide(...)` calls. Construct
/// via [`Resolver::load`] which handles `$REPO` substitution and
/// optional per-tool overlay loading.
#[derive(Debug)]
pub struct Resolver {
    profile: Profile,
    /// Per-tool overlay keyed by *file stem* (e.g. `rg`, not `/usr/bin/rg`).
    /// In Step 1 only the requested tool's overlay (if any) is
    /// loaded; later commits can pre-load every overlay in the
    /// directory for `sy policy show`.
    tool_overlays: BTreeMap<String, Profile>,
}

impl Resolver {
    /// Load `configs/policy/profiles/<profile_name>.toml` rooted at
    /// `policy_root` (typically `<repo>/configs/policy`). If `tool`
    /// is provided and `configs/policy/tools/<tool>.toml` exists,
    /// that file is parsed as an overlay. `repo` replaces every
    /// literal `$REPO` segment in `read_paths` / `write_paths`.
    pub fn load(
        policy_root: &Path,
        profile_name: &str,
        tool: Option<&str>,
        repo: &Path,
    ) -> Result<Self> {
        let profile_path = policy_root
            .join("profiles")
            .join(format!("{profile_name}.toml"));
        let mut profile = parse_profile(&profile_path)?;
        expand_repo(&mut profile, repo);

        let mut tool_overlays = BTreeMap::new();
        if let Some(name) = tool {
            let overlay_path = policy_root.join("tools").join(format!("{name}.toml"));
            if overlay_path.exists() {
                let mut overlay = parse_profile(&overlay_path)?;
                expand_repo(&mut overlay, repo);
                tool_overlays.insert(name.to_string(), overlay);
            }
        }

        Ok(Self {
            profile,
            tool_overlays,
        })
    }

    /// Decide whether `tool` invoked with `argv` should be allowed,
    /// denied, or paused for consent. See the module-head comment for
    /// the three-way rule.
    pub fn decide(&self, tool: &str, argv: &[String]) -> Decision {
        let profile = self.effective_profile(tool);

        if profile.exec_allowlist.is_empty() {
            return Decision::Deny;
        }

        match matches_allowlist(&profile.exec_allowlist, tool, argv) {
            Some(_entry) => match profile.require_consent {
                ConsentMode::Never | ConsentMode::OncePerSession => Decision::Allow,
                ConsentMode::EveryCall => Decision::ConsentRequired {
                    reason: format!("policy requires consent on every call to {tool}"),
                },
            },
            None => Decision::ConsentRequired {
                reason: format!("{tool} is not in the allowlist for this profile"),
            },
        }
    }

    /// SHA-256 (hex) of the canonical-serialised resolved policy.
    /// Stamped on every audit-log record in Step 5; included in
    /// `sy policy show --json` for `diff`able review.
    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(canonical_bytes(&self.profile));
        for (tool, overlay) in &self.tool_overlays {
            hasher.update(tool.as_bytes());
            hasher.update([0u8]);
            hasher.update(canonical_bytes(overlay));
        }
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }

    /// Return a clone of the profile that `decide(tool, …)` would
    /// consult. Step 3's `sandbox::fork_and_exec` needs the resolved
    /// `Profile` shape (with `$REPO` expanded and any per-tool
    /// overlay applied) to drive the Landlock ruleset builder.
    pub fn effective(&self, tool: &str) -> Profile {
        self.effective_profile(tool).clone()
    }

    fn effective_profile(&self, tool: &str) -> &Profile {
        let key = Path::new(tool)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(tool);
        self.tool_overlays.get(key).unwrap_or(&self.profile)
    }
}

/// One-shot helper used by Step 3's `agt sandbox-exec` entry point.
/// Loads the named profile + optional tool overlay and returns the
/// resolved `Profile` (with `$REPO` expanded), bypassing the
/// `Resolver` decision surface. Keeps `Profile` itself a pure data
/// type with no resolver dependency cycle.
pub fn resolve_profile(
    policy_root: &Path,
    profile_name: &str,
    tool: Option<&str>,
    repo: &Path,
) -> Result<Profile> {
    let resolver = Resolver::load(policy_root, profile_name, tool, repo)?;
    Ok(resolver.effective(tool.unwrap_or("")))
}

/// Locate the `policy/` directory at runtime. Used by both
/// `resolve_spawn_command` (parent process,
/// daemon-side) and `sandbox_exec` /
/// `sandbox_run` (sandboxed child re-exec) so the daemon and the
/// `systemd-run --user --scope -- sy agt sandbox-exec` child agree
/// on which `profiles/` directory holds the active policy.
///
/// Search order matches what `sy apply` produces:
///   1. `$XDG_CONFIG_HOME/policy` (default `~/.config/policy`)
///      — productized location written by `sy apply` from
///      `configs/policy/`.
///   2. `<cwd>/configs/policy` — in-repo dev path, so running
///      `sy agt run` from a sy checkout uses the source-of-truth
///      profiles without an apply step.
///   3. Walk parents of `cwd` looking for a `configs/policy`
///      directory — handles running from a subdir of a repo.
///
/// `systemd-run --user --scope` defaults the scope's cwd to `/`,
/// so without (1) the child re-exec resolved `policy_root` to
/// `/configs/policy/` and failed `read /configs/policy/profiles/
/// normal.toml: No such file or directory`. This is the canonical
/// recovery point.
pub fn resolve_policy_root(cwd: &Path) -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        let candidate = xdg.join("policy");
        if candidate.join("profiles").is_dir() {
            return Ok(candidate);
        }
    }
    let mut here: Option<&Path> = Some(cwd);
    while let Some(d) = here {
        let candidate = d.join("configs").join("policy");
        if candidate.join("profiles").is_dir() {
            return Ok(candidate);
        }
        here = d.parent();
    }
    anyhow::bail!(
        "no policy/profiles directory found under $XDG_CONFIG_HOME or any ancestor of {} \
         — run `sy apply` to install configs/policy/",
        cwd.display()
    )
}

fn parse_profile(path: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str::<Profile>(&text).with_context(|| format!("parse {}", path.display()))
}

fn expand_repo(profile: &mut Profile, repo: &Path) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for path in profile
        .read_paths
        .iter_mut()
        .chain(profile.write_paths.iter_mut())
    {
        *path = substitute_repo(path, repo, home.as_deref());
    }
}

/// Substitute `$REPO` (the daemon-side cwd / agent workspace) and
/// `$HOME` (the running user's home, derived from the `HOME` env
/// var). Anchored to component boundaries so a literal segment like
/// `$REPOSITORY` is NOT a partial match — only an exact path
/// component named `$REPO` is replaced.
fn substitute_repo(path: &Path, repo: &Path, home: Option<&Path>) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.iter() {
        if component == "$REPO" {
            out.push(repo);
        } else if component == "$HOME" {
            if let Some(h) = home {
                out.push(h);
            } else {
                out.push(component);
            }
        } else {
            out.push(component);
        }
    }
    out
}

fn matches_allowlist<'a>(
    allowlist: &'a [ExecAllow],
    tool: &str,
    argv: &[String],
) -> Option<&'a ExecAllow> {
    let argv_joined = argv.join(" ");
    allowlist
        .iter()
        .find(|entry| bin_matches(&entry.bin, tool) && argv_glob_matches(&entry.argv, &argv_joined))
}

fn bin_matches(pattern: &Path, tool: &str) -> bool {
    let Some(pattern_str) = pattern.to_str() else {
        return false;
    };
    if pattern_str == tool {
        return true;
    }
    build_glob(pattern_str)
        .map(|g| g.compile_matcher().is_match(tool))
        .unwrap_or(false)
}

fn argv_glob_matches(globs: &[String], argv_joined: &str) -> bool {
    globs.iter().any(|pattern| {
        build_glob(pattern)
            .map(|g| g.compile_matcher().is_match(argv_joined))
            .unwrap_or(false)
    })
}

fn build_glob(pattern: &str) -> Option<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(false)
        .build()
        .ok()
}

fn canonical_bytes(profile: &Profile) -> Vec<u8> {
    // toml::to_string is stable for ordered structs since serde
    // visits fields in declaration order; that's the canonical form
    // we hash. Any future field reorder is captured by the test
    // `fingerprint_changes_on_overlay` blowing up on regenerate.
    toml::to_string(profile).unwrap_or_default().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRICT: &str = "strict";
    const NORMAL: &str = "normal";

    fn policy_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("configs")
            .join("policy")
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
    }

    #[test]
    fn strict_denies_everything() {
        let r = Resolver::load(&policy_root(), STRICT, None, &repo_root()).expect("load strict");
        let argv = vec!["foo".to_string()];
        assert_eq!(r.decide("rg", &argv), Decision::Deny);
    }

    #[test]
    fn normal_allows_rg() {
        let r = Resolver::load(&policy_root(), NORMAL, None, &repo_root()).expect("load normal");
        let argv = vec!["foo".to_string()];
        assert_eq!(r.decide("/usr/bin/rg", &argv), Decision::Allow);
    }

    #[test]
    fn normal_consents_for_unknown_tool() {
        let r = Resolver::load(&policy_root(), NORMAL, None, &repo_root()).expect("load normal");
        let argv = vec!["https://example.com".to_string()];
        match r.decide("/usr/bin/curl", &argv) {
            Decision::ConsentRequired { reason } => {
                assert!(!reason.is_empty(), "consent reason should be populated");
            }
            other => panic!("expected ConsentRequired, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_changes_on_overlay() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Clone the shipped normal profile into the tempdir so we
        // can layer a synthetic overlay alongside it without touching
        // the canonical configs/policy/ tree.
        let profiles_dir = tmp.path().join("profiles");
        let tools_dir = tmp.path().join("tools");
        std::fs::create_dir_all(&profiles_dir).expect("mkdir profiles");
        std::fs::create_dir_all(&tools_dir).expect("mkdir tools");
        let canonical = policy_root().join("profiles").join("normal.toml");
        std::fs::copy(&canonical, profiles_dir.join("normal.toml")).expect("copy normal");

        let alone = Resolver::load(tmp.path(), NORMAL, None, &repo_root()).expect("load alone");

        let overlay = r#"
read_paths = []
write_paths = []
exec_allowlist = [{ bin = "/usr/bin/rg", argv = ["only-this"] }]
net_outbound_allowlist = []
env_passthrough_allowlist = []
max_runtime_seconds = 10
max_stdout_bytes = 1024
max_memory_mb = 64
max_pids = 8
deny_network = true
require_consent = "every_call"
"#;
        std::fs::write(tools_dir.join("rg.toml"), overlay).expect("write overlay");
        let layered =
            Resolver::load(tmp.path(), NORMAL, Some("rg"), &repo_root()).expect("load layered");

        assert_ne!(alone.fingerprint(), layered.fingerprint());
    }
}
