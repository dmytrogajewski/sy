//! ACP (Agent Client Protocol) wire layer.
//!
//! One ACP child = one stdio JSON-RPC 2.0 connection. We classify each
//! inbound line by which JSON-RPC fields are present:
//!   * has `id` + `result|error`            → response to one of our requests
//!   * has `id` + `method`                  → reverse request from agent
//!   * no `id`, has `method`                → notification

use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot, Mutex},
};

use crate::agt::registry::AgentSpec;

#[derive(Debug)]
pub enum AcpInbound {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Closed,
}

pub struct AcpChild {
    write_tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    next_id: AtomicU64,
    pub inbound: mpsc::Receiver<AcpInbound>,
    _child: Child,
}

impl AcpChild {
    pub async fn spawn(spec: &AgentSpec, cwd: &Path) -> Result<Self> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn ACP child: {}", spec.command))?;

        let stdin = child.stdin.take().context("child stdin missing")?;
        let stdout = child.stdout.take().context("child stdout missing")?;
        let stderr = child.stderr.take().context("child stderr missing")?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::channel::<AcpInbound>(256);
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);

        // Writer task: serializes stdin writes.
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(buf) = write_rx.recv().await {
                if stdin.write_all(&buf).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // Reader task: classifies each line.
        let pending_r = pending.clone();
        let inbound_r = inbound_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("acp: malformed line: {e}: {trimmed}");
                        continue;
                    }
                };
                let id = v.get("id").cloned();
                let method = v.get("method").and_then(|m| m.as_str()).map(str::to_owned);
                let has_result = v.get("result").is_some();
                let err = v.get("error").cloned();

                match (id, method, has_result || err.is_some()) {
                    (Some(idv), None, true) => {
                        let key = idv.as_u64().unwrap_or(0);
                        let mut p = pending_r.lock().await;
                        if let Some(tx) = p.remove(&key) {
                            let payload = if let Some(e) = err {
                                Err(e.to_string())
                            } else {
                                Ok(v.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = tx.send(payload);
                        }
                    }
                    (Some(idv), Some(m), false) => {
                        let params = v.get("params").cloned().unwrap_or(Value::Null);
                        let _ = inbound_r
                            .send(AcpInbound::Request {
                                id: idv,
                                method: m,
                                params,
                            })
                            .await;
                    }
                    (None, Some(m), _) => {
                        let params = v.get("params").cloned().unwrap_or(Value::Null);
                        let _ = inbound_r
                            .send(AcpInbound::Notification { method: m, params })
                            .await;
                    }
                    _ => {}
                }
            }
            let _ = inbound_r.send(AcpInbound::Closed).await;
        });

        // Stderr passthrough task: log to our stderr prefixed with agent name.
        let label = spec.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{label}] {line}");
            }
        });

        Ok(Self {
            write_tx,
            pending,
            next_id: AtomicU64::new(1),
            inbound: inbound_rx,
            _child: child,
        })
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_line(&msg).await?;
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("acp error: {e}")),
            Err(_) => Err(anyhow!("acp request {method}: receiver dropped")),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_line(&msg).await
    }

    pub async fn respond(&self, id: Value, result: Result<Value>) -> Result<()> {
        let msg = match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": e.to_string()}
            }),
        };
        self.send_line(&msg).await
    }

    async fn send_line(&self, msg: &Value) -> Result<()> {
        let mut buf = serde_json::to_vec(msg)?;
        buf.push(b'\n');
        self.write_tx
            .send(buf)
            .await
            .map_err(|_| anyhow!("acp child stdin closed"))
    }
}
