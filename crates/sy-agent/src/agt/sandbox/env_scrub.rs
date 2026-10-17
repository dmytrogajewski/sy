//! Pure environment scrubber — drops every variable outside the
//! per-profile allowlist before `Command::envs(scrubbed)` is called.
//! No I/O; safe to call from any context including the `pre_exec`
//! closure (although the binary uses it in the parent before spawn).
//!
//! SPEC §4.4 step 3 layer "env scrub". The `normal` profile allows
//! `PATH`, `HOME`, `LANG`, `TERM`; `strict` allows nothing
//! (everything below `env_clear()` is supplied by the spawned binary
//! itself). Sensitive variables (`SY_API_KEY`, `OPENAI_API_KEY`,
//! `AWS_*`, …) drop silently rather than logging, to avoid leaking
//! their names into the audit log.

use std::collections::HashMap;

/// Return a new map containing only the entries from `env` whose
/// keys appear in `allowlist`. Comparison is exact (case-sensitive)
/// — matching how `Command::env` treats keys on Linux. An empty
/// `allowlist` yields an empty map.
pub fn scrub(env: &HashMap<String, String>, allowlist: &[String]) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(allowlist.len());
    for key in allowlist {
        if let Some(value) = env.get(key) {
            out.insert(key.clone(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_keeps_only_allowlist() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/x".to_string());
        env.insert("SECRET_TOKEN".to_string(), "shhh".to_string());

        let allowlist = vec!["PATH".to_string(), "HOME".to_string()];
        let out = scrub(&env, &allowlist);

        assert_eq!(out.len(), 2);
        assert_eq!(out.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(out.get("HOME").map(String::as_str), Some("/home/x"));
        assert!(
            !out.contains_key("SECRET_TOKEN"),
            "secret must not survive scrub"
        );
    }
}
