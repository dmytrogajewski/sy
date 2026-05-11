//! Synchronous IPC client used by `sy agt list/prompt/stop/run/diag`.
//! Short-lived: open socket, send one ClientReq line, read replies, close.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};

use crate::agt::{
    protocol::{exit, ClientReply, ClientReq},
    socket_path, AgtError,
};

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect() -> Result<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path).map_err(|e| {
            AgtError {
                code: exit::DAEMON_UNAVAILABLE,
                msg: format!(
                    "connect {} (is sy-agentd running? `sy agt diag` to check): {e}",
                    path.display()
                ),
            }
        })?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let writer = stream.try_clone().map_err(|e| AgtError {
            code: exit::DAEMON_UNAVAILABLE,
            msg: format!("clone socket: {e}"),
        })?;
        let reader = BufReader::new(stream);
        Ok(Self { reader, writer })
    }

    pub fn send(&mut self, req: &ClientReq) -> Result<()> {
        let line = serde_json::to_string(req)? + "\n";
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn recv(&mut self) -> Result<ClientReply> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(anyhow!("daemon closed connection"));
        }
        let reply: ClientReply = serde_json::from_str(line.trim_end())
            .with_context(|| format!("parse daemon reply: {}", line.trim_end()))?;
        Ok(reply)
    }

    /// Send a request and block for one reply.
    pub fn round_trip(&mut self, req: &ClientReq) -> Result<ClientReply> {
        self.send(req)?;
        self.recv()
    }
}

/// Read replies until EOF or a non-Event reply arrives.
/// Used by `sy agt tail --follow` and the inspector.
#[allow(dead_code)]
pub fn stream_events(client: &mut Client, mut on_event: impl FnMut(ClientReply) -> bool) -> Result<()> {
    loop {
        match client.recv() {
            Ok(reply) => {
                if !on_event(reply) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}
