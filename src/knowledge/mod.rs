//! sy knowledge — system-wide semantic-search layer.
//!
//! Architecture: Qdrant (vector DB, supervised child of `sy knowledge
//! daemon`) plus fastembed-rs in-process (CUDA EP, falls back to CPU).
//! Sources are declarative entries in sy.toml; index metadata lives at
//! $XDG_STATE_HOME/sy/knowledge/. CLI subcommands and the MCP server can
//! both query Qdrant directly even without the daemon running — the
//! daemon owns the *write side* (indexing) and the qdrant subprocess.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

pub mod autoconfig;
pub mod chunk;
pub mod cli;
pub mod daemon;
pub mod embed;
pub mod extract;
pub mod ipc;
pub mod manifest;
pub mod mcp;
pub mod normalize;
pub mod qdrant;
pub mod runctx;
pub mod sources;
pub mod state;
pub mod status;

#[derive(Subcommand)]
pub enum KnowledgeCmd {
    /// Run the long-lived foreground daemon (spawned by niri at startup).
    /// Supervises qdrant, watches registered sources, runs scheduled syncs.
    Daemon,

    /// Register a path (file or directory) as an index source. Edits sy.toml.
    Add {
        /// Path to a file or folder to index.
        path: PathBuf,
        /// Mark the entry as disabled at insert time.
        #[arg(long)]
        disabled: bool,
        /// Treat the path as a *discovery root*: walk it (recursively) for
        /// `qdr.toml` files and let each manifested folder declare its own
        /// indexing rules. Without this flag the path is indexed wholesale.
        #[arg(long)]
        discover: bool,
    },

    /// Remove a registered source (matches by path).
    Rm { path: PathBuf },

    /// List registered sources + last-indexed timestamps.
    List {
        #[arg(long)]
        json: bool,
    },

    /// One-shot incremental index (re-walks all sources, embeds new/changed
    /// chunks, removes deleted ones). Talks to Qdrant directly; works
    /// without the daemon.
    Index {
        /// Restrict to a single registered source path.
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },

    /// Force re-embed everything: drop the collection and re-index every
    /// source. Use after switching embedding model.
    Sync {
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },

    /// Show or set the daemon's incremental-sync interval. With no arg,
    /// prints the current setting; with an arg like `30m`, writes it back
    /// to sy.toml.
    Schedule { interval: Option<String> },

    /// Semantic search over indexed content.
    Search {
        query: String,
        #[arg(short = 'k', long, default_value = "8")]
        limit: usize,
        #[arg(long)]
        json: bool,
        /// Restrict to a registered source path prefix.
        #[arg(long)]
        source: Option<PathBuf>,
    },

    /// List active `qdr.toml` manifests (auto-discovered + under explicit
    /// discover roots). Invalid manifests are reported under `errors`.
    Manifests {
        #[arg(long)]
        json: bool,
    },

    /// Emit one JSON line for the waybar `custom/sy-knowledge` module.
    /// Reads the daemon's status snapshot; collapses the tile when no
    /// daemon is running.
    Waybar,

    /// Print the daemon's status snapshot (human or `--json`).
    Status {
        #[arg(long)]
        json: bool,
    },

    /// Fuzzel-driven interactive search. Prompts for a query, shows hits,
    /// opens the chosen file via xdg-open. Useful as a waybar right-click.
    Pick,

    /// Stop the daemon from firing scheduled / FS-tickle / IPC IndexNow
    /// passes until `resume`. User-driven `index/sync/search` still work.
    Pause,

    /// Resume the daemon (clears the pause flag, runs one catch-up pass).
    Resume,

    /// Idempotent toggle (used by waybar middle-click).
    TogglePause,

    /// Cancel any in-flight pass cooperatively. Daemon stays paused if it
    /// was paused before. Files already embedded keep their qdrant points.
    Cancel,

    /// Throughput probe: embed N dummy chunks, report chunks/s, batch ms,
    /// and the active embed backend (cuda | cpu).
    Bench {
        #[arg(long, default_value_t = 256)]
        n: usize,
        #[arg(long)]
        json: bool,
    },

    /// Stdio JSON-RPC MCP server exposing knowledge tools to agents.
    Mcp,

    /// Register `sy-knowledge` MCP server in every supported agent's
    /// config (Claude Code, Cursor, Codex, Gemini). Sets
    /// `[knowledge].mcp_enabled = true` in sy.toml. Dry-run by default.
    McpEnable {
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        json: bool,
    },

    /// Remove `sy-knowledge` from every supported agent's MCP config.
    /// Sets `[knowledge].mcp_enabled = false` in sy.toml. Dry-run by
    /// default.
    McpDisable {
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        json: bool,
    },

    /// Show whether `sy-knowledge` is registered in each agent's MCP
    /// config (read-only).
    McpStatus {
        #[arg(long)]
        json: bool,
    },
}

pub fn dispatch(cmd: KnowledgeCmd) -> Result<()> {
    match cmd {
        KnowledgeCmd::Daemon => daemon::run(),
        KnowledgeCmd::Add {
            path,
            disabled,
            discover,
        } => cli::add(&path, disabled, discover),
        KnowledgeCmd::Rm { path } => cli::rm(&path),
        KnowledgeCmd::List { json } => cli::list(json),
        KnowledgeCmd::Index { source, json } => cli::index(source.as_deref(), json),
        KnowledgeCmd::Sync { yes } => cli::sync(yes),
        KnowledgeCmd::Schedule { interval } => cli::schedule(interval.as_deref()),
        KnowledgeCmd::Search {
            query,
            limit,
            json,
            source,
        } => cli::search(&query, limit, json, source.as_deref()),
        KnowledgeCmd::Manifests { json } => cli::manifests(json),
        KnowledgeCmd::Waybar => cli::waybar(),
        KnowledgeCmd::Status { json } => cli::status_cmd(json),
        KnowledgeCmd::Pick => cli::pick(),
        KnowledgeCmd::Pause => cli::pause(),
        KnowledgeCmd::Resume => cli::resume(),
        KnowledgeCmd::TogglePause => cli::toggle_pause(),
        KnowledgeCmd::Cancel => cli::cancel_op(),
        KnowledgeCmd::Bench { n, json } => cli::bench(n, json),
        KnowledgeCmd::Mcp => mcp::run(),
        KnowledgeCmd::McpEnable { apply, json } => cli::mcp_enable(apply, json),
        KnowledgeCmd::McpDisable { apply, json } => cli::mcp_disable(apply, json),
        KnowledgeCmd::McpStatus { json } => cli::mcp_status_cmd(json),
    }
}

/// Stable exit codes for `sy knowledge` (CLIG: documented in plan).
#[derive(Debug)]
pub struct KnowledgeError {
    pub code: i32,
    pub msg: String,
}

impl std::fmt::Display for KnowledgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::error::Error for KnowledgeError {}

pub mod exit {
    pub const SOURCE_NOT_FOUND: i32 = 3;
    pub const QDRANT_UNREACHABLE: i32 = 4;
    pub const EMBEDDING_FAILED: i32 = 5;
    #[allow(dead_code)]
    pub const ALREADY_RUNNING: i32 = 6;
    #[allow(dead_code)] // raised by future hard-failure paths in cli::run_index
    pub const INDEX_FAILED: i32 = 7;
    #[allow(dead_code)] // surfaced via daemon eprintln + `manifests --json` errors[]
    pub const MANIFEST_INVALID: i32 = 8;
}

/// Vector dimension for the chosen embedding model (multilingual-e5-base,
/// post-NPU migration). Existing 1024-dim collections from the
/// multilingual-e5-large era must be dropped and re-indexed:
///   sy knowledge cancel
///   sy knowledge drop
///   sy knowledge resync
pub const VECTOR_DIM: usize = 768;

/// Default Qdrant REST port (bind 127.0.0.1).
pub const QDRANT_PORT: u16 = 6333;

/// Default sync interval if [knowledge].schedule is unset.
pub const DEFAULT_SCHEDULE: &str = "15m";

/// Qdrant collection name owned by sy.
pub const COLLECTION: &str = "sy_knowledge";
