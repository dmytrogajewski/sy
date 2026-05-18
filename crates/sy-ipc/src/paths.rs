//! Endpoint name → socket path resolution for `sy ipc ping` /
//! `sy ipc describe` (ROADMAP arch-ipc-v1 Step 7).
//!
//! Centralises the mapping that previously lived inline inside each
//! daemon's `socket_path()`. Keeping the convention in one place lets
//! `sy ipc <endpoint>` and `sy doctor` (Zone 6) speak the same names
//! end-to-end.

use std::env;
use std::path::PathBuf;

/// All endpoint names `sy ipc` recognises. Kept in alphabetical order
/// for stable `--help` output.
pub const ENDPOINTS: &[&str] = &["agt", "aiplane", "knowledge", "stack"];

/// Resolve an endpoint name to its canonical UDS path. Returns `None`
/// for unknown names so the CLI can surface "valid endpoints: …"
/// rather than silently dispatching to a wrong daemon.
///
/// Path layout (matching each daemon's own `socket_path()`):
///   * `knowledge`, `aiplane` → `$XDG_RUNTIME_DIR/sy-knowledge.sock`
///     (aiplane multiplexes on the knowledge listener per Step 5)
///   * `agt`                  → `$XDG_RUNTIME_DIR/sy-agentd.sock`
///   * `stack`                → `$XDG_RUNTIME_DIR/sy/stackbar.sock`
pub fn for_endpoint(name: &str) -> Option<PathBuf> {
    let runtime = runtime_dir();
    match name {
        "knowledge" | "aiplane" => Some(runtime.join("sy-knowledge.sock")),
        "agt" => Some(runtime.join("sy-agentd.sock")),
        "stack" => Some(runtime.join("sy").join("stackbar.sock")),
        _ => None,
    }
}

fn runtime_dir() -> PathBuf {
    if let Ok(d) = env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let uid = unsafe { libc_getuid() };
    PathBuf::from(format!("/run/user/{uid}"))
}

extern "C" {
    fn getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    getuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock around `XDG_RUNTIME_DIR` mutation so the four path tests
    /// can't trip over each other when cargo's default parallel
    /// scheduler runs them on different threads of the same process.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_runtime_dir<F: FnOnce()>(dir: &str, f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", dir);
        f();
        match prev {
            Some(v) => env::set_var("XDG_RUNTIME_DIR", v),
            None => env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn ping_endpoint_resolution_knowledge() {
        // SPEC §4.7: `sy ipc ping knowledge` resolves to
        // `$XDG_RUNTIME_DIR/sy-knowledge.sock`. Locked down here so a
        // future rename of the socket can't silently break the doctor
        // recipe.
        with_runtime_dir("/tmp/sy-runtime-test", || {
            assert_eq!(
                for_endpoint("knowledge"),
                Some(PathBuf::from("/tmp/sy-runtime-test/sy-knowledge.sock"))
            );
        });
    }

    #[test]
    fn ping_endpoint_resolution_aiplane_aliases_knowledge() {
        // Step 5 multiplexes aiplane and knowledge on one socket; the
        // CLI must reflect that alias so `sy ipc ping aiplane` doesn't
        // try to open a non-existent `sy-aiplane.sock`.
        with_runtime_dir("/tmp/sy-runtime-test", || {
            assert_eq!(for_endpoint("aiplane"), for_endpoint("knowledge"));
        });
    }

    #[test]
    fn ping_endpoint_resolution_agt_and_stack() {
        with_runtime_dir("/tmp/sy-runtime-test", || {
            assert_eq!(
                for_endpoint("agt"),
                Some(PathBuf::from("/tmp/sy-runtime-test/sy-agentd.sock"))
            );
            assert_eq!(
                for_endpoint("stack"),
                Some(PathBuf::from("/tmp/sy-runtime-test/sy/stackbar.sock"))
            );
        });
    }

    #[test]
    fn unknown_endpoint_returns_none() {
        with_runtime_dir("/tmp/sy-runtime-test", || {
            assert!(for_endpoint("nonsense").is_none());
        });
    }
}
