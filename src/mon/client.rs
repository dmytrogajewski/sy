//! Tiny client over the `sy mon collect` aggregator's IPC surface.
//!
//! Wraps [`sy_ipc::client::Client`] so the CLI and the MCP server share
//! one retry loop and one error vocabulary. The connect path retries
//! 100 ms × up to 10 attempts (≈ 1 s total budget) so a fresh
//! `sy mon snapshot` invocation that races a `sy-mon-collect.service`
//! restart still succeeds — per SPEC §6 risk "race with aggregator
//! restart".
//!
//! Anything beyond `snapshot`/`history` lives behind the same
//! [`Client`] re-export so future consumers (sy-mon Step 16 popup,
//! `sy doctor`) don't grow their own retry shims.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sy_core::mon::snapshot::SystemSnapshot;
use sy_ipc::client::{CallOpts, Client};
use sy_ipc::envelope::Response;

/// Systemd unit operators run `systemctl --user start <UNIT>` against
/// when the aggregator is down. Surfaced in error messages so an agent
/// or human reading stderr has a one-paste-fix.
pub const AGGREGATOR_UNIT: &str = "sy-mon-collect.service";

const METHOD_SNAPSHOT: &str = "system.mon.snapshot";
const METHOD_HISTORY: &str = "system.mon.history";

/// Connect-retry budget per SPEC §6 risk "race with aggregator
/// restart": 10 attempts × 100 ms between attempts = ~1 s cap.
const CONNECT_RETRY_ATTEMPTS: u32 = 10;
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Connect to the aggregator at `bind_path`, retrying transient
/// `ConnectionRefused`/`NotFound`/`AddrNotAvailable` errors per the
/// 100 ms × 10 budget. Any other I/O error fails fast; if every retry
/// is exhausted the final error is returned with the socket path and
/// the unit name baked into the message.
pub async fn connect_with_retry(bind_path: &Path) -> Result<Client> {
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..CONNECT_RETRY_ATTEMPTS {
        match Client::connect(bind_path).await {
            Ok(c) => return Ok(c),
            Err(e) if is_transient(&e) => {
                last_err = Some(e);
                tokio::time::sleep(CONNECT_RETRY_INTERVAL).await;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "connect aggregator socket {} (is {AGGREGATOR_UNIT} running?)",
                        bind_path.display()
                    )
                });
            }
        }
    }
    let err = last_err.unwrap_or_else(|| std::io::Error::other("connect retry budget exhausted"));
    Err(err).with_context(|| {
        format!(
            "connect aggregator socket {} after {CONNECT_RETRY_ATTEMPTS} attempts; \
             is {AGGREGATOR_UNIT} running?",
            bind_path.display()
        )
    })
}

fn is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::NotFound
            | std::io::ErrorKind::AddrNotAvailable
    )
}

/// Fetch the latest `SystemSnapshot` from the aggregator at
/// `bind_path`. Caller owns retry semantics beyond connect — once the
/// connection is live the call itself does not retry (the aggregator's
/// `system.mon.snapshot` is non-blocking).
pub async fn snapshot(bind_path: &Path) -> Result<SystemSnapshot> {
    let mut client = connect_with_retry(bind_path).await?;
    let resp = client
        .call(METHOD_SNAPSHOT, serde_json::json!({}), CallOpts::default())
        .await
        .with_context(|| format!("call {METHOD_SNAPSHOT}"))?;
    match resp {
        Response::Ok { result, .. } => {
            serde_json::from_value(result).context("deserialise SystemSnapshot")
        }
        Response::Err { error, .. } => Err(anyhow!(
            "{METHOD_SNAPSHOT} returned error {:?}: {}",
            error.code,
            error.message
        )),
    }
}

/// Fetch a ring-buffer history window for `metric` over `seconds`
/// seconds. Returns `(captured_at_ms, value)` pairs oldest-first per
/// the SPEC §4 wire shape.
pub async fn history(bind_path: &Path, metric: &str, seconds: u32) -> Result<Vec<(u64, f32)>> {
    let mut client = connect_with_retry(bind_path).await?;
    let params = serde_json::json!({ "metric": metric, "seconds": seconds });
    let resp = client
        .call(METHOD_HISTORY, params, CallOpts::default())
        .await
        .with_context(|| format!("call {METHOD_HISTORY}"))?;
    match resp {
        Response::Ok { result, .. } => {
            serde_json::from_value(result["samples"].clone()).context("deserialise history samples")
        }
        Response::Err { error, .. } => Err(anyhow!(
            "{METHOD_HISTORY} returned error {:?}: {}",
            error.code,
            error.message
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ENOENT` (socket file missing because the aggregator hasn't
    /// bound yet) and `ECONNREFUSED` (socket exists from a previous
    /// crash but no listener) both surface as `NotFound` /
    /// `ConnectionRefused`; the retry loop must treat both as
    /// transient so a fresh `sy mon snapshot` races the supervisor
    /// without spurious failures.
    #[test]
    fn transient_kinds_cover_aggregator_race() {
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::AddrNotAvailable,
        ] {
            let e = std::io::Error::new(kind, "test");
            assert!(is_transient(&e), "kind {kind:?} should be transient");
        }
    }

    /// `PermissionDenied` is a hard failure (the socket file's mode is
    /// 0600 per SPEC §4 Security — retrying won't change uid). Pin it
    /// here so a regression that broadened `is_transient` is caught.
    #[test]
    fn permission_denied_is_not_transient() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "EACCES");
        assert!(!is_transient(&e));
    }
}
