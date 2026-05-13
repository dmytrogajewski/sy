//! sy stack — temporary-artifact bar with three pools (clip / app / user).
//!
//! Architecture: state on disk (`$XDG_STATE_HOME/sy/stack/items.json` + blobs)
//! is the source of truth. CLI subcommands mutate state and signal the
//! `sy stack bar` daemon over a Unix socket so the bar repaints; the daemon
//! never holds canonical state in memory.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

pub mod bar;
pub mod cli;
pub mod clip;
pub mod ipc;
pub mod mcp;
pub mod onto;
pub mod state;

/// Item pool. Clipboard entries are a live read-only mirror of cliphist
/// and are NEVER stored in items.json — the bar fetches them on tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    App,
    User,
}

impl Kind {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "app" => Ok(Kind::App),
            "user" => Ok(Kind::User),
            other => Err(anyhow::anyhow!(
                "unknown stack kind: {other} (expected app|user)"
            )),
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::User => "user",
        }
    }
}

#[derive(Subcommand)]
pub enum StackCmd {
    /// Push an item onto the stack. ITEM is a path, or `-` to read from stdin.
    Push {
        /// `-` reads stdin as content; otherwise treated as a path.
        item: String,
        /// Pool: app | user (default: user).
        #[arg(long, default_value = "user")]
        kind: String,
        /// Optional human-readable name for the item.
        #[arg(long)]
        name: Option<String>,
        /// Print the planned action and exit without mutating state.
        #[arg(long)]
        dry_run: bool,
        /// Print the new item id on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Remove the most recent item from a pool and print its id.
    Pop {
        /// Pool: app | user (default: user).
        #[arg(long, default_value = "user")]
        kind: String,
        /// Pop a specific id instead of the top.
        #[arg(long)]
        id: Option<String>,
    },
    /// List items (default: human table; --json for machine output).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print an item's payload to stdout (file contents or stored text).
    Preview { id: String },
    /// Remove an item by id.
    Remove { id: String },
    /// Move an item's payload into a target directory and remove it from the stack.
    Move {
        id: String,
        /// Destination directory.
        dest: PathBuf,
    },
    /// Print a stable filesystem path for an item (materialising content into a temp file).
    Link { id: String },
    /// Hand the item to a configured integration (`[[stack.onto]]` in sy.toml).
    Onto {
        /// Integration name (must match `name = ` in sy.toml).
        integration: String,
        id: String,
    },
    /// Run a context-menu action on an item (called by the bar daemon).
    Action {
        /// Item id (the bar resolves slot → id before calling).
        id: String,
        /// Action: copy | preview | move | link | onto | agent | remove
        action: String,
        /// Where the id lives: `stack` (items.json) or `clip` (cliphist).
        /// Defaults to stack.
        #[arg(long, default_value = "stack")]
        source: String,
    },
    /// Show or hide the bar (sends to daemon; falls back to PID-toggle).
    Toggle,
    /// Run the iced layer-shell bar daemon (foreground; spawned by niri at startup).
    Bar,
    /// Run the stdio JSON-RPC MCP server exposing stack tools to agents.
    Mcp,
}

pub fn dispatch(cmd: StackCmd) -> Result<()> {
    match cmd {
        StackCmd::Push {
            item,
            kind,
            name,
            dry_run,
            json,
        } => cli::push(&item, &kind, name.as_deref(), dry_run, json),
        StackCmd::Pop { kind, id } => cli::pop(&kind, id.as_deref()),
        StackCmd::List { json } => cli::list(json),
        StackCmd::Preview { id } => cli::preview(&id),
        StackCmd::Remove { id } => cli::remove(&id),
        StackCmd::Move { id, dest } => cli::move_to(&id, &dest),
        StackCmd::Link { id } => cli::link(&id),
        StackCmd::Onto { integration, id } => onto::run(&integration, &id),
        StackCmd::Action { id, action, source } => cli::action(&id, &action, &source),
        StackCmd::Toggle => cli::toggle(),
        StackCmd::Bar => bar::run(),
        StackCmd::Mcp => mcp::run(),
    }
}

/// Stable exit codes for `sy stack` (CLIG: documented in plan).
#[derive(Debug)]
pub struct StackError {
    pub code: i32,
    pub msg: String,
}

impl std::fmt::Display for StackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::error::Error for StackError {}

pub mod exit {
    pub const NOT_FOUND: i32 = 3;
    pub const INTEGRATION_FAILED: i32 = 4;
}
