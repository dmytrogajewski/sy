//! Policy schema — serde data shapes for the TOML profile files.
//!
//! Glob semantics (used in [`ExecAllow::argv`] and [`ExecAllow::bin`]):
//! - `"*"` matches every argument string (including the empty one).
//! - `"test*"` matches anything starting with the literal `test`.
//! - An empty list (`[]`) matches nothing — the strict profile leans
//!   on this to express "deny everything" by construction.
//!
//! Matching is delegated to `globset::GlobBuilder`. We disable
//! backslash escapes and treat the separator as non-significant so a
//! pattern like `test*` matches argv `test-with-dashes/and/slashes`
//! the way an operator intuitively expects.
//!
//! `$REPO` substitution in `read_paths` / `write_paths` happens in
//! `resolver::Resolver::load`, not here — the schema is pure data.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    pub read_paths: Vec<PathBuf>,
    #[serde(default)]
    pub write_paths: Vec<PathBuf>,
    #[serde(default)]
    pub exec_allowlist: Vec<ExecAllow>,
    #[serde(default)]
    pub net_outbound_allowlist: Vec<NetAllow>,
    #[serde(default)]
    pub env_passthrough_allowlist: Vec<String>,
    #[serde(default)]
    pub max_runtime_seconds: u64,
    #[serde(default)]
    pub max_stdout_bytes: u64,
    #[serde(default)]
    pub max_memory_mb: u64,
    #[serde(default)]
    pub max_pids: u64,
    #[serde(default)]
    pub deny_network: bool,
    #[serde(default)]
    pub require_consent: ConsentMode,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecAllow {
    /// Absolute binary path, or `*` to match every binary (trusted
    /// profile only).
    pub bin: PathBuf,
    /// argv glob patterns (see module head comment).
    pub argv: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetAllow {
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsentMode {
    Never,
    #[default]
    OncePerSession,
    EveryCall,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const STRICT_PATH: &str = "configs/policy/profiles/strict.toml";
    const NORMAL_PATH: &str = "configs/policy/profiles/normal.toml";
    const MIN_NORMAL_EXEC_ENTRIES: usize = 3;

    fn load_workspace_toml(rel: &str) -> Profile {
        let path = workspace_root().join(rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
    }

    fn workspace_root() -> PathBuf {
        // CARGO_MANIFEST_DIR points at the crate the test belongs to;
        // for the root `sy` crate that already is the workspace root.
        Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
    }

    #[test]
    fn strict_profile_round_trip() {
        let profile = load_workspace_toml(STRICT_PATH);
        assert!(profile.deny_network, "strict must deny_network");
        let re = toml::to_string(&profile).expect("serialize");
        let back: Profile = toml::from_str(&re).expect("re-parse");
        assert_eq!(profile, back);
    }

    #[test]
    fn normal_profile_round_trip() {
        let profile = load_workspace_toml(NORMAL_PATH);
        assert!(
            profile.exec_allowlist.len() >= MIN_NORMAL_EXEC_ENTRIES,
            "normal needs at least {MIN_NORMAL_EXEC_ENTRIES} exec_allowlist entries (rg/cargo/git)"
        );
        let re = toml::to_string(&profile).expect("serialize");
        let back: Profile = toml::from_str(&re).expect("re-parse");
        assert_eq!(profile, back);
    }
}
