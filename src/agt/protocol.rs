use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind")]
pub enum ClientReq {
    Run {
        agent: String,
        cwd: PathBuf,
        prompt: String,
    },
    List,
    Prompt {
        session_id: String,
        text: String,
    },
    Stop {
        session_id: String,
    },
    Tail {
        session_id: String,
        follow: bool,
        replay: bool,
    },
    PermissionDecision {
        request_id: String,
        allow: bool,
    },
    Diag,
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind")]
pub enum ClientReply {
    RunReply {
        session_id: String,
    },
    ListReply {
        sessions: Vec<SessionInfo>,
    },
    Ack,
    DiagReply {
        agents: Vec<DiagEntry>,
    },
    /// Streaming event from the daemon (tail). The inner `event` keeps its
    /// own `kind` discriminator under a separate field so we don't collide
    /// with the outer `ClientReply` tag.
    Event {
        event: DaemonEvent,
    },
    Error {
        message: String,
        code: u16,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind")]
pub enum DaemonEvent {
    Transcript {
        session_id: String,
        entry: TranscriptEntry,
        ts: String,
    },
    Status {
        session_id: String,
        status: SessionStatus,
    },
    Permission {
        session_id: String,
        request_id: String,
        summary: String,
        body: String,
    },
    Closed {
        session_id: String,
        reason: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub agent: String,
    pub cwd: PathBuf,
    pub status: SessionStatus,
    pub created_at: String,
    pub last_activity: String,
    pub summary: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Working,
    Awaiting,
    Stopped { code: i32 },
    Error { msg: String },
}

impl SessionStatus {
    pub fn label(&self) -> &'static str {
        match self {
            SessionStatus::Starting => "starting",
            SessionStatus::Running => "running",
            SessionStatus::Working => "working",
            SessionStatus::Awaiting => "awaiting",
            SessionStatus::Stopped { .. } => "stopped",
            SessionStatus::Error { .. } => "error",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntry {
    AgentText { text: String },
    UserText { text: String },
    ToolCall { tool: String, input: Value },
    ToolResult { tool: String, output: Value, ok: bool },
    Plan { items: Vec<String> },
    Status { msg: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiagEntry {
    pub name: String,
    pub command: String,
    pub found: bool,
    pub version: String,
}

/// Stable exit codes per CLAUDE.md (CLIG: meaningful, non-zero on failure).
/// 0 = success and 2 = clap usage error are handled implicitly by the runtime
/// and clap respectively, so they don't need named constants here.
pub mod exit {
    pub const DAEMON_UNAVAILABLE: i32 = 1;
    pub const REGISTRY: i32 = 3;
    pub const NO_SESSION: i32 = 4;
}

