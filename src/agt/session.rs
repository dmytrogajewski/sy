//! Per-session state: ACP child handle, transcript (memory + jsonl on disk),
//! subscribers for live events.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

use crate::agt::{
    acp::AcpChild,
    protocol::{DaemonEvent, SessionInfo, SessionStatus, TranscriptEntry},
};

pub struct Session {
    pub info: SessionInfo,
    pub child: Arc<Mutex<AcpChild>>,
    pub acp_session_id: String,
    pub transcript: Vec<(String, TranscriptEntry)>,
    pub subscribers: Vec<mpsc::Sender<DaemonEvent>>,
    pub jsonl: Option<File>,
    pub dir: PathBuf,
}

impl Session {
    pub fn new(
        id: String,
        agent: String,
        cwd: PathBuf,
        prompt_summary: String,
        child: AcpChild,
        acp_session_id: String,
    ) -> Result<Self> {
        let dir = state_dir().join(&id);
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let jsonl_path = dir.join("transcript.jsonl");
        let jsonl = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl_path)
            .ok();
        // Persist the agent-side ACP session id for future resume.
        let _ = std::fs::write(dir.join(".acp-session-id"), &acp_session_id);

        let now = Utc::now().to_rfc3339();
        let info = SessionInfo {
            id,
            agent,
            cwd,
            status: SessionStatus::Starting,
            created_at: now.clone(),
            last_activity: now,
            summary: prompt_summary,
        };
        Ok(Self {
            info,
            child: Arc::new(Mutex::new(child)),
            acp_session_id,
            transcript: Vec::new(),
            subscribers: Vec::new(),
            jsonl,
            dir,
        })
    }

    pub fn append(&mut self, entry: TranscriptEntry) {
        let ts = Utc::now().to_rfc3339();
        self.info.last_activity = ts.clone();

        if let Some(f) = self.jsonl.as_mut() {
            if let Ok(line) = serde_json::to_string(&serde_json::json!({
                "ts": ts,
                "entry": entry,
            })) {
                let _ = writeln!(f, "{}", line);
            }
        }

        let event = DaemonEvent::Transcript {
            session_id: self.info.id.clone(),
            entry: entry.clone(),
            ts: ts.clone(),
        };
        self.transcript.push((ts, entry));
        self.broadcast(event);
        self.persist_meta();
    }

    pub fn set_status(&mut self, status: SessionStatus) -> Option<Completion> {
        let prev = std::mem::replace(&mut self.info.status, status.clone());
        self.info.last_activity = Utc::now().to_rfc3339();
        self.broadcast(DaemonEvent::Status {
            session_id: self.info.id.clone(),
            status,
        });
        self.persist_meta();
        completion_kind(&prev, &self.info.status)
    }

    pub fn broadcast(&mut self, event: DaemonEvent) {
        self.subscribers
            .retain(|tx| tx.try_send(event.clone()).is_ok());
    }

    pub fn subscribe(&mut self) -> mpsc::Receiver<DaemonEvent> {
        let (tx, rx) = mpsc::channel(256);
        self.subscribers.push(tx);
        rx
    }

    pub fn replay(&self) -> Vec<DaemonEvent> {
        self.transcript
            .iter()
            .map(|(ts, e)| DaemonEvent::Transcript {
                session_id: self.info.id.clone(),
                entry: e.clone(),
                ts: ts.clone(),
            })
            .collect()
    }

    fn persist_meta(&self) {
        if let Ok(s) = serde_json::to_string_pretty(&self.info) {
            let _ = std::fs::write(self.dir.join("meta.json"), s);
        }
    }
}

/// What kind of "agent finished doing something" event a status transition
/// represents. Used by the daemon to decide whether to fire a desktop
/// notification.
#[derive(Debug, Clone, Copy)]
pub enum Completion {
    /// Working → Running: the agent finished its turn and is idle.
    TurnDone,
    /// Any → Stopped: session has ended cleanly.
    Stopped,
    /// Any → Error: session ended with a failure.
    Errored,
}

fn completion_kind(prev: &SessionStatus, next: &SessionStatus) -> Option<Completion> {
    match (prev, next) {
        (SessionStatus::Working, SessionStatus::Running) => Some(Completion::TurnDone),
        (p, SessionStatus::Stopped { .. }) if !matches!(p, SessionStatus::Stopped { .. }) => {
            Some(Completion::Stopped)
        }
        (p, SessionStatus::Error { .. }) if !matches!(p, SessionStatus::Error { .. }) => {
            Some(Completion::Errored)
        }
        _ => None,
    }
}

pub fn state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/sy/agt")
}

/// Normalize an ACP `session/update` notification's `update` field into a
/// transcript entry. Unknown shapes become a generic Status entry.
pub fn entry_from_update(update: &Value) -> Option<TranscriptEntry> {
    let kind = update.get("sessionUpdate").and_then(|v| v.as_str())?;
    match kind {
        "agent_message_chunk" | "agent_thought_chunk" => {
            let text = update
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            if text.is_empty() {
                None
            } else {
                Some(TranscriptEntry::AgentText { text })
            }
        }
        "user_message_chunk" => {
            let text = update
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            if text.is_empty() {
                None
            } else {
                Some(TranscriptEntry::UserText { text })
            }
        }
        "tool_call" | "tool_call_update" => {
            let tool = update
                .get("title")
                .or_else(|| update.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("tool")
                .to_string();
            let input = update.get("rawInput").cloned().unwrap_or(Value::Null);
            let raw_output = update.get("rawOutput").cloned();
            let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status == "completed" || raw_output.is_some() {
                Some(TranscriptEntry::ToolResult {
                    tool,
                    output: raw_output.unwrap_or(Value::Null),
                    ok: status != "failed",
                })
            } else {
                Some(TranscriptEntry::ToolCall { tool, input })
            }
        }
        "plan" => {
            let items = update
                .get("entries")
                .and_then(|e| e.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            e.get("content")
                                .and_then(|c| c.as_str())
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(TranscriptEntry::Plan { items })
        }
        other => Some(TranscriptEntry::Status {
            msg: format!("update: {other}"),
        }),
    }
}
