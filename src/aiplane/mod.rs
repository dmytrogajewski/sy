//! `aiplane` — multi-workload NPU plane.
//!
//! Generalises what used to be the embedding-only knowledge daemon into
//! a substrate that can host any number of NPU-eligible workloads
//! (embedding, reranking, VAD, STT, TTS, OCR, CLIP, denoise, eye-track).
//! One daemon process owns `/dev/accel/accel0`; everyone else
//! (`sy knowledge search`, the MCP server, ad-hoc CLI invocations)
//! sends work over a Unix socket via `ipc::request(Req::Run { … })`.
//!
//! ## Module surface
//!
//! - `registry` — `WorkloadKind` enum + `Workload` trait + `Registry`
//!   dispatch.
//! - `session` — shared NPU mutex + `RunCtx` (cancellation/throughput).
//! - `reexec` — the AMD venv re-exec dance (called from `main()` before
//!   any thread spawn).
//! - `status` — `Status` JSON snapshot at
//!   `$XDG_STATE_HOME/sy/aiplane/status.json` + waybar refresh signal.
//! - `ipc` — Unix-socket protocol: fire-and-forget `Op` + request-
//!   response `Req`/`Resp`.
//! - `workloads` — per-workload `Workload` impls + `register_all`.
//!
//! ## Status during the migration
//!
//! As of the aiplane-scaffold commit, the daemon (`sy knowledge
//! daemon`, `sy-knowledge.service`) still lives under
//! `src/knowledge/daemon.rs` and uses the in-tree `knowledge::ipc` /
//! `knowledge::embed`. This `aiplane::` module compiles in parallel
//! and is exercised by unit tests. A follow-up commit lifts the
//! daemon and renames the systemd unit to `sy-aiplane.service`.

pub mod cli;
pub mod error;
pub mod ipc;
#[cfg(feature = "mon-exporter")]
pub mod mon_exporter;
pub mod reexec;
pub mod registry;
pub mod scheduler;
pub mod session;
pub mod status;
pub mod supervisor;
pub mod warm_pool;
pub mod worker;
pub mod worker_ipc;
pub mod workloads;

/// Shared process-wide mutex for tests that mutate `XDG_RUNTIME_DIR`
/// (or other globals — `XDG_STATE_HOME`, `SY_*` env vars, the NPU
/// `/dev/accel/accel0` device handle). All daemon-in-thread /
/// worker-in-thread / socket-binding tests acquire this so they don't
/// cross-route requests when cargo runs them in parallel. Modules
/// outside `aiplane::` (e.g. `syauth`, `power::cli`, `doctor::checks`)
/// re-export this as their own lock so every env-touching test in the
/// `sy` bin shares one rendezvous point.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    //! arch-observability Step 2 ratchet: production code under
    //! `src/aiplane/` and `src/knowledge/daemon.rs` must not call
    //! `eprintln!` or `println!` directly — those bypass the
    //! `tracing` subscriber installed by `sy_core::obs::init` and so
    //! don't make it into journald or the rolling JSONL appender
    //! (SPEC §4.6). The two permitted classes of `println!` callers
    //! are (a) `aiplane::cli` (user-facing primary output on stdout
    //! per CLIG) and (b) `#[cfg(test)]` test code; everything else
    //! must use `tracing::{info,warn,error}!`.

    use std::fs;
    use std::path::{Path, PathBuf};

    /// Files allowed to use `println!` because they are CLI-direct
    /// user output sites (CLIG: primary output on stdout). The
    /// supervisor / worker / daemon code paths still must NOT
    /// `println!`.
    const CLI_PRINTLN_ALLOWLIST: &[&str] = &["src/aiplane/cli.rs"];

    /// Scan `path` (recursively if a directory) for lines that begin
    /// — modulo leading whitespace — with `eprintln!(` or `println!(`,
    /// skipping `#[cfg(test)]` modules and string-literal occurrences
    /// in doc comments. Returns `(file, line_no, snippet)` tuples.
    fn scan_print_macros(path: &Path) -> Vec<(PathBuf, usize, String)> {
        let mut hits = Vec::new();
        let mut stack = vec![path.to_path_buf()];
        while let Some(p) = stack.pop() {
            if p.is_dir() {
                let Ok(rd) = fs::read_dir(&p) else { continue };
                for ent in rd.flatten() {
                    stack.push(ent.path());
                }
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(src) = fs::read_to_string(&p) else {
                continue;
            };
            let mut in_test_mod = false;
            let mut brace_depth_at_test_entry: i32 = 0;
            let mut brace_depth: i32 = 0;
            for (idx, raw) in src.lines().enumerate() {
                let line = raw.trim_start();
                // Skip block-/line-comments and doc comments at the
                // start of the line — the trim already dropped leading
                // whitespace; we only need to ignore `///`/`//` lines.
                if line.starts_with("//") {
                    continue;
                }
                // Cheap `#[cfg(test)]` detector. The follow-up `mod`
                // block opens with a `{`; we track brace depth until
                // it closes.
                if !in_test_mod && line.starts_with("#[cfg(test)]") {
                    in_test_mod = true;
                    brace_depth_at_test_entry = brace_depth;
                }
                for ch in line.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                    } else if ch == '}' {
                        brace_depth -= 1;
                        if in_test_mod && brace_depth <= brace_depth_at_test_entry {
                            in_test_mod = false;
                        }
                    }
                }
                if in_test_mod {
                    continue;
                }
                if line.starts_with("eprintln!(") || line.starts_with("println!(") {
                    hits.push((p.clone(), idx + 1, raw.to_string()));
                }
            }
        }
        hits
    }

    fn repo_root() -> PathBuf {
        // CARGO_MANIFEST_DIR points at the crate that owns this test —
        // the `sy` bin lives at the repo root, so the same path is the
        // workspace root.
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn no_eprintln_left_in_aiplane_or_knowledge_daemon() {
        let root = repo_root();
        let aiplane = root.join("src/aiplane");
        let knowledge_daemon = root.join("src/knowledge/daemon.rs");

        let mut offenders: Vec<(PathBuf, usize, String)> = Vec::new();
        offenders.extend(scan_print_macros(&aiplane));
        offenders.extend(scan_print_macros(&knowledge_daemon));

        // Strip the CLI-output allowlist. The strip uses repo-relative
        // path components so the test passes from any CWD.
        offenders.retain(|(p, _, raw)| {
            let rel = p.strip_prefix(&root).unwrap_or(p);
            let rel_s = rel.to_string_lossy();
            let cli_allowed = CLI_PRINTLN_ALLOWLIST
                .iter()
                .any(|&allowed| rel_s == allowed);
            // The aiplane CLI file is allowed to `println!` for primary
            // user output, but never `eprintln!` — all diagnostics must
            // flow through `tracing`.
            !(cli_allowed && raw.trim_start().starts_with("println!("))
        });

        assert!(
            offenders.is_empty(),
            "found {} forbidden `eprintln!`/`println!` call(s) in production code:\n{}",
            offenders.len(),
            offenders
                .iter()
                .map(|(p, n, raw)| format!("  {}:{} {}", p.display(), n, raw.trim()))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
