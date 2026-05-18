//! syauth waybar applet.
//!
//! Surfaces the 6-digit LESC numeric-comparison code the privileged
//! desktop `syauth pair --waybar` process is currently waiting on,
//! and lets the operator accept / reject the bond with a click. The
//! two processes rendezvous over `$XDG_RUNTIME_DIR/syauth/`:
//!
//! - `pair-request.json` — written by `syauth pair --waybar` when
//!   BlueZ asks for user confirmation of the LESC numeric comparison.
//!   Schema:
//!
//!   ```json
//!   {
//!     "schema_version": 1,
//!     "kind": "pair_confirm",
//!     "request_id": "<pid>-<nanos>",
//!     "passkey": "692386",
//!     "created_at_secs": 1779039123
//!   }
//!   ```
//!
//! - `pair-response.json` — written by this applet on click. Schema:
//!
//!   ```json
//!   {
//!     "schema_version": 1,
//!     "request_id": "<matching>",
//!     "decision": "accept" | "reject"
//!   }
//!   ```
//!
//! Subcommands:
//!   sy syauth --waybar       → emit waybar JSON for the bar
//!   sy syauth accept         → write `accept` response
//!   sy syauth reject         → write `reject` response

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

const IPC_SUBDIR: &str = "syauth";
const REQUEST_FILE: &str = "pair-request.json";
const RESPONSE_FILE: &str = "pair-response.json";

/// Schema version both sides write + read. Bump together with the
/// matching constant in `crates/syauth-cli/src/pair_backend.rs`.
const SCHEMA_VERSION: u32 = 1;

/// Decoded pair-confirm request from the desktop side.
#[derive(Debug, Deserialize)]
struct PairRequest {
    schema_version: u32,
    kind: String,
    request_id: String,
    passkey: String,
    created_at_secs: u64,
}

/// Decision the applet writes back on click.
#[derive(Debug, Serialize)]
struct PairResponse<'a> {
    schema_version: u32,
    request_id: &'a str,
    decision: &'a str,
}

pub fn run(action: Option<&str>, waybar: bool) -> Result<()> {
    if waybar {
        return waybar_out();
    }
    match action.unwrap_or("status") {
        "accept" => respond("accept"),
        "reject" => respond("reject"),
        "status" => print_status(),
        other => Err(anyhow!(
            "unknown syauth action: {other} (accept|reject|status; --waybar for bar JSON)"
        )),
    }
}

// -- paths -----------------------------------------------------------------

fn ipc_dir() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR")?;
    if xdg.is_empty() {
        return None;
    }
    Some(PathBuf::from(xdg).join(IPC_SUBDIR))
}

fn request_path() -> Option<PathBuf> {
    ipc_dir().map(|d| d.join(REQUEST_FILE))
}

fn response_path() -> Option<PathBuf> {
    ipc_dir().map(|d| d.join(RESPONSE_FILE))
}

// -- request load ---------------------------------------------------------

/// Try to read + parse the current request file. Returns `None` if
/// the file is absent or unparseable. Schema-version mismatches are
/// dropped silently — the bar slot just stays empty so an out-of-
/// date applet doesn't pretend to confirm something it can't.
fn read_request() -> Option<PairRequest> {
    let p = request_path()?;
    let bytes = fs::read(&p).ok()?;
    let req: PairRequest = serde_json::from_slice(&bytes).ok()?;
    if req.schema_version != SCHEMA_VERSION || req.kind != "pair_confirm" {
        return None;
    }
    tracing::debug!(
        target: "sy::syauth",
        request_id = %req.request_id,
        created_at_secs = req.created_at_secs,
        "read pair request"
    );
    Some(req)
}

// -- waybar --------------------------------------------------------------

fn waybar_out() -> Result<()> {
    let Some(req) = read_request() else {
        // No pending request → empty slot.
        println!(r#"{{"text":"","tooltip":""}}"#);
        return Ok(());
    };
    let text = format!("syauth:{}", req.passkey);
    let tip = format!(
        "syauth pair — 6-digit code {}\\nclick: accept · right-click: reject",
        req.passkey
    );
    println!(
        r#"{{"text":"{text}","class":"pending","tooltip":"{tip}"}}"#,
        text = text,
        tip = tip,
    );
    Ok(())
}

// -- accept / reject ------------------------------------------------------

fn respond(decision: &str) -> Result<()> {
    let Some(req) = read_request() else {
        notify("syauth: no pending pair request");
        return Ok(());
    };
    let Some(out_path) = response_path() else {
        return Err(anyhow!("XDG_RUNTIME_DIR unset; cannot write response"));
    };
    let dir = out_path
        .parent()
        .ok_or_else(|| anyhow!("response path has no parent"))?;
    fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let body = serde_json::to_vec(&PairResponse {
        schema_version: SCHEMA_VERSION,
        request_id: &req.request_id,
        decision,
    })?;
    write_atomic(&out_path, &body)
        .with_context(|| format!("write {}", out_path.display()))?;
    notify(&format!("syauth: {decision} (passkey {})", req.passkey));
    Ok(())
}

/// Atomically write `body` to `path` (write-then-rename pattern). The
/// desktop polls the response file and reads the moment it appears;
/// an interrupted partial write would surface as an empty/truncated
/// JSON which the desktop's hand-rolled parser would interpret as
/// "no decision yet" and keep polling. write_atomic eliminates that
/// race entirely.
fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent"))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pair-response")
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(body)?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn notify(body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "sy", "-t", "1500", "syauth", body])
        .status();
}

// -- status (debug / lifecycle) -------------------------------------------

fn print_status() -> Result<()> {
    match read_request() {
        Some(req) => {
            println!("syauth pair: pending");
            println!("  passkey: {}", req.passkey);
            println!("  request_id: {}", req.request_id);
            println!("  ipc-dir: {}", ipc_dir().unwrap().display());
        }
        None => {
            println!("syauth pair: idle");
            if let Some(d) = ipc_dir() {
                println!("  ipc-dir: {}", d.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waybar_emits_empty_slot_when_no_request_file() {
        // We can't easily intercept stdout here, so just exercise
        // the read_request path: with no XDG_RUNTIME_DIR or no file,
        // read_request returns None.
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        // Point XDG_RUNTIME_DIR at a tempdir with no request file.
        let td = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", td.path());
        }
        assert!(read_request().is_none());
        // Restore env.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    #[test]
    fn waybar_reads_valid_request_file() {
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join(IPC_SUBDIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(REQUEST_FILE),
            br#"{"schema_version":1,"kind":"pair_confirm","request_id":"abc","passkey":"123456","created_at_secs":1700000000}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", td.path());
        }
        let req = read_request().expect("must parse");
        assert_eq!(req.passkey, "123456");
        assert_eq!(req.request_id, "abc");
        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    #[test]
    fn waybar_rejects_unknown_schema_version() {
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join(IPC_SUBDIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(REQUEST_FILE),
            br#"{"schema_version":99,"kind":"pair_confirm","request_id":"x","passkey":"000000","created_at_secs":0}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", td.path());
        }
        assert!(read_request().is_none());
        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }
}
