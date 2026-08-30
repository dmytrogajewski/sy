//! `sy knowledge {add|rm|list|index|sync|schedule|search}` impls.
//!
//! Most commands are pure functions of disk state (sy.toml + index.json +
//! Qdrant). They work without the daemon running. After mutating sy.toml
//! they fire a non-blocking IPC notification to the daemon if it's up.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde_json::json;

use super::{
    embed, eval, exit, extract, ipc, manifest,
    pipeline::{self, Record},
    qdrant::{self, Point, PointPayload},
    repair,
    runctx::RunCtx,
    sources::{self, SourceKind, SourceMode},
    sparse, state, status, transcribe,
};

const UPSERT_BATCH: usize = 64;

pub fn add(path: &Path, disabled: bool, discover: bool) -> Result<()> {
    let abs = sources::expand(&path.display().to_string()).unwrap_or_else(|_| path.to_path_buf());
    if !abs.exists() {
        return Err(super::KnowledgeError {
            code: exit::SOURCE_NOT_FOUND,
            msg: format!("path not found: {}", abs.display()),
        }
        .into());
    }
    let mode = if discover {
        SourceMode::Discover
    } else {
        SourceMode::Explicit
    };
    let added = sources::add(path, disabled, mode)?;
    let label = match mode {
        SourceMode::Explicit => "",
        SourceMode::Discover => " [discover]",
    };
    if added {
        println!("+ {}{}", abs.display(), label);
        sources::notify_daemon_refresh();
    } else {
        println!("= {} (already registered){}", abs.display(), label);
    }
    Ok(())
}

pub fn rm(path: &Path) -> Result<()> {
    let removed = sources::remove(path)?;
    let abs = sources::expand(&path.display().to_string()).unwrap_or_else(|_| path.to_path_buf());
    if removed {
        println!("- {}", abs.display());
        sources::notify_daemon_refresh();
    } else {
        println!("? {} (not registered)", abs.display());
    }
    Ok(())
}

pub fn list(json_out: bool) -> Result<()> {
    let section = sources::load()?;
    let idx = state::load().unwrap_or_default();
    let qdrant_count = qdrant::point_count().unwrap_or(0);
    let discovered = manifest::discover_all();

    if json_out {
        let entries: Vec<_> = section
            .sources
            .iter()
            .map(|s| {
                let resolved = sources::expand(&s.path).unwrap_or_else(|_| PathBuf::from(&s.path));
                let last_indexed = idx
                    .files
                    .iter()
                    .filter(|(p, _)| p.starts_with(&resolved.display().to_string()))
                    .map(|(_, e)| e.mtime)
                    .max()
                    .unwrap_or(0);
                let mode = match s.mode {
                    SourceMode::Explicit => "explicit",
                    SourceMode::Discover => "discover",
                };
                json!({
                    "path": s.path,
                    "resolved": resolved.display().to_string(),
                    "enabled": s.enabled,
                    "mode": mode,
                    "last_indexed_unix": last_indexed,
                })
            })
            .collect();
        let discovered_json: Vec<_> = discovered
            .iter()
            .map(|m| {
                json!({
                    "name": m.name,
                    "folder": m.folder.display().to_string(),
                    "enabled": m.enabled,
                    "tags": m.tags,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schedule": section.schedule.unwrap_or_else(|| super::DEFAULT_SCHEDULE.into()),
                "qdrant_points": qdrant_count,
                "discover_home": sources::discover_home_enabled(),
                "sources": entries,
                "discovered": discovered_json,
            }))?
        );
        return Ok(());
    }
    println!(
        "schedule: {}    qdrant_points: {}    discover_home: {}",
        section
            .schedule
            .as_deref()
            .unwrap_or(super::DEFAULT_SCHEDULE),
        qdrant_count,
        sources::discover_home_enabled()
    );
    println!();
    if section.sources.is_empty() && discovered.is_empty() {
        println!("(no sources registered — try `sy knowledge add <path>` or drop a qdr.toml in a folder)");
        return Ok(());
    }
    if !section.sources.is_empty() {
        println!("{:<3} {:<8} PATH", "EN", "MODE");
        for s in &section.sources {
            let resolved = sources::expand(&s.path).unwrap_or_else(|_| PathBuf::from(&s.path));
            let mark = if s.enabled { "y" } else { "-" };
            let mode = match s.mode {
                SourceMode::Explicit => "explicit",
                SourceMode::Discover => "discover",
            };
            println!(
                "{:<3} {:<8} {}  ({})",
                mark,
                mode,
                s.path,
                resolved.display()
            );
        }
    }
    if !discovered.is_empty() {
        println!();
        println!(
            "discovered ({} qdr.toml manifest{})",
            discovered.len(),
            if discovered.len() == 1 { "" } else { "s" }
        );
        for m in &discovered {
            let mark = if m.enabled { "y" } else { "-" };
            println!("{:<3} {}  [{}]", mark, m.folder.display(), m.name);
        }
    }
    Ok(())
}

pub fn manifests(json_out: bool) -> Result<()> {
    let manifests = manifest::discover_all();
    if json_out {
        let arr: Vec<_> = manifests
            .iter()
            .map(|m| {
                json!({
                    "name": m.name,
                    "folder": m.folder.display().to_string(),
                    "enabled": m.enabled,
                    "include": m.include,
                    "exclude": m.exclude,
                    "max_depth": m.max_depth,
                    "max_file_bytes": m.max_file_bytes,
                    "respect_gitignore": m.respect_gitignore,
                    "follow_symlinks": m.follow_symlinks,
                    "schedule": m.schedule,
                    "tags": m.tags,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    if manifests.is_empty() {
        println!("(no qdr.toml manifests found)");
        return Ok(());
    }
    for m in &manifests {
        let mark = if m.enabled { "y" } else { "-" };
        println!("{:<3} {}  [{}]", mark, m.folder.display(), m.name);
        if !m.include.is_empty() {
            println!("    include: {:?}", m.include);
        }
        if !m.exclude.is_empty() {
            println!("    exclude: {:?}", m.exclude);
        }
        if !m.tags.is_empty() {
            println!("    tags:    {:?}", m.tags);
        }
        if let Some(s) = &m.schedule {
            println!("    schedule: {s}");
        }
    }
    Ok(())
}

/// Like `human_count` but with thousands separators rather than the
/// 1.2k / 12k / 1.2M bucketing used in the waybar tile. Reads better
/// inside the tooltip's tags table.
fn human_count_full(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// One-line JSON for the waybar `custom/sy-knowledge` module. Reads the
/// status file the daemon writes; falls back to an empty/hidden tile
/// when the daemon hasn't written one in the last 90 s.
pub fn waybar() -> Result<()> {
    let st = status::load().ok();
    let payload = match st {
        None => json!({"text": "", "class": "hidden", "tooltip": ""}),
        Some(s) if !status::is_fresh(&s) || !s.daemon_running => {
            let tooltip = format!(
                "sy knowledge — daemon down\\nlast status {}s ago",
                state::now_secs().saturating_sub(s.ts_unix)
            );
            json!({"text": "", "class": "hidden", "tooltip": tooltip})
        }
        Some(s) => waybar_payload(&s),
    };
    // Manual single-line print: waybar parses one JSON per stdout line.
    println!("{}", serde_json::to_string(&payload)?);
    Ok(())
}

fn waybar_payload(s: &status::Status) -> serde_json::Value {
    let glyph = "🧠";
    let class = if s.paused {
        "paused"
    } else if s.cancelling {
        "cancelling"
    } else if s.last_error.is_some() {
        "error"
    } else if s.indexing {
        "indexing"
    } else {
        "idle"
    };
    let prefix = match class {
        "indexing" => format!("{glyph} ⟳ "),
        "cancelling" => format!("{glyph} ⟳ "),
        "paused" => format!("{glyph} ⏸ "),
        "error" => format!("{glyph} ! "),
        _ => format!("{glyph} "),
    };
    let text = format!("{prefix}{}", human_count(s.points));
    let tooltip = build_tooltip(s);
    json!({"text": text, "class": class, "tooltip": tooltip, "alt": class})
}

fn human_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Multi-section Pango tooltip (waybar renders Pango markup in tooltips,
/// just like the clock's `tooltip-format`). Wrapped in `<tt>` so the
/// tags column lines up. Includes:
///   • daemon state + schedule + last-sync row
///   • per-tag chunk counts (qdrant facet over the `tags` payload index)
///   • a brief "how to search" hint
fn build_tooltip(s: &status::Status) -> String {
    let now = state::now_secs();
    let next_in = s.next_run_unix.saturating_sub(now);
    let last_ago = if s.last_index_at_unix > 0 {
        format!(
            "{} ago",
            human_secs(now.saturating_sub(s.last_index_at_unix))
        )
    } else {
        "never".into()
    };
    let state_line = if s.paused {
        "paused".to_string()
    } else if s.cancelling {
        "cancelling…".to_string()
    } else if s.indexing {
        "indexing now…".to_string()
    } else if let Some(e) = s.last_error.as_ref() {
        format!("error: {}", truncate(e, 60))
    } else {
        "idle".to_string()
    };
    let qd = if s.qdrant_ready { "ready" } else { "down" };
    let tput = match s.last_throughput_chunks_per_s {
        Some(v) if v > 0.0 => format!("{:.0}/s", v),
        _ => "—".into(),
    };
    let cap = match s.cpu_max_percent {
        Some(p) if p > 0 => format!("{p}%"),
        _ => "off".into(),
    };

    let manifest_extra = if s.manifests_disabled > 0 {
        format!(" ({} disabled)", s.manifests_disabled)
    } else {
        String::new()
    };

    // Tags facet — best-effort. Empty list when qdrant is down or the
    // index isn't built yet; tooltip still renders without the section.
    let mut tags = qdrant::facet_tags(32).unwrap_or_default();
    tags.sort_by_key(|(_, count)| std::cmp::Reverse(*count));

    let mut out = String::new();
    out.push_str("<tt>");
    out.push_str(&pango_escape(&format!("sy knowledge — {state_line}\n")));
    out.push_str(&pango_escape(&format!(
        "schedule:   {}   (next in {})\n",
        human_secs(s.schedule_secs),
        human_secs(next_in)
    )));
    out.push_str(&pango_escape(&format!(
        "sources:    {} discover, {} explicit\n",
        s.sources_discover, s.sources_explicit
    )));
    out.push_str(&pango_escape(&format!(
        "manifests:  {} active{manifest_extra}\n",
        s.manifests_active
    )));
    out.push_str(&pango_escape(&format!(
        "points:     {} (qdrant {qd})\n",
        human_count_full(s.points)
    )));
    let hw = if s.embed_hardware.is_empty() {
        String::new()
    } else {
        format!(" ({})", s.embed_hardware)
    };
    out.push_str(&pango_escape(&format!(
        "embed:      {}{hw} · {} · cpu cap {}\n",
        s.embed_backend, tput, cap
    )));
    out.push_str(&pango_escape(&format!(
        "last sync:  {last_ago}, {} indexed / {} deleted ({}ms)\n",
        s.last_index_indexed, s.last_index_deleted, s.last_index_ms
    )));

    if !tags.is_empty() {
        out.push_str(&pango_escape("\nTAGS              CHUNKS\n"));
        let pad = 16usize;
        for (tag, count) in tags.iter().take(12) {
            let tag_disp = if tag.chars().count() > pad {
                truncate(tag, pad)
            } else {
                format!("{:<width$}", tag, width = pad)
            };
            out.push_str(&pango_escape(&format!(
                "  {tag_disp}{:>9}\n",
                human_count_full(*count)
            )));
        }
        if tags.len() > 12 {
            out.push_str(&pango_escape(&format!("  … {} more\n", tags.len() - 12)));
        }
    }

    out.push_str(&pango_escape(
        "\nSEARCH\n  CLI:    sy knowledge search \"<query>\"\n  Fuzzy:  left-click 🧠 → fuzzel prompt\n  Agents: sy-knowledge MCP\n",
    ));
    out.push_str(&pango_escape("\nleft: search · middle: pause"));
    out.push_str("</tt>");
    out
}

/// Pango-markup escape: only the four characters that have special
/// meaning inside `<tt>…</tt>`. Keeps newlines verbatim so waybar
/// renders them as line breaks.
fn pango_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn human_secs(secs: u64) -> String {
    if secs == 0 {
        "now".into()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Pretty/JSON dump of the daemon's status snapshot. Convenience for
/// scripts that don't want to format waybar JSON themselves.
pub fn status_cmd(json_out: bool) -> Result<()> {
    let s = match status::load() {
        Ok(s) => s,
        Err(_) => {
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"daemon_running": false}))?
                );
            } else {
                println!("(daemon down — no status file)");
            }
            return Ok(());
        }
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }
    let now = state::now_secs();
    let age = now.saturating_sub(s.ts_unix);
    println!("ts:           {}s ago", age);
    println!(
        "daemon:       {}",
        if s.daemon_running && status::is_fresh(&s) {
            "running"
        } else {
            "down"
        }
    );
    println!(
        "qdrant:       {}",
        if s.qdrant_ready { "ready" } else { "down" }
    );
    println!(
        "schedule:     {}   (next in {})",
        human_secs(s.schedule_secs),
        human_secs(s.next_run_unix.saturating_sub(now))
    );
    println!(
        "sources:      {} discover, {} explicit",
        s.sources_discover, s.sources_explicit
    );
    println!(
        "manifests:    {} active, {} disabled",
        s.manifests_active, s.manifests_disabled
    );
    println!("points:       {}", s.points);
    println!(
        "state:        {}",
        if s.paused {
            "paused"
        } else if s.cancelling {
            "cancelling"
        } else if s.indexing {
            "indexing"
        } else {
            "idle"
        }
    );
    println!("embed:        {}", s.embed_backend);
    if !s.embed_hardware.is_empty() {
        println!("hardware:     {}", s.embed_hardware);
    }
    if let Some(t) = s.last_throughput_chunks_per_s {
        println!("throughput:   {:.1} chunks/s", t);
    }
    match s.cpu_max_percent {
        Some(p) if p > 0 => println!("cpu cap:      {p}%"),
        _ => println!("cpu cap:      off"),
    }
    if s.last_index_at_unix > 0 {
        println!(
            "last sync:    {} ago — indexed {} / chunks {} / skipped {} / deleted {} ({}ms)",
            human_secs(now.saturating_sub(s.last_index_at_unix)),
            s.last_index_indexed,
            s.last_index_chunks,
            s.last_index_skipped,
            s.last_index_deleted,
            s.last_index_ms
        );
    }
    if let Some(e) = &s.last_error {
        println!("last error:   {e}");
    }
    Ok(())
}

/// Send `Op::Pause`/`Resume`/`TogglePause`/`Cancel` to the daemon. Each
/// is fire-and-forget; if the daemon isn't running, the IPC layer logs
/// nothing and we surface a hint on stderr.
pub fn pause() -> Result<()> {
    send_or_warn(&ipc::Op::Pause, "pause")
}
pub fn resume() -> Result<()> {
    send_or_warn(&ipc::Op::Resume, "resume")
}
pub fn toggle_pause() -> Result<()> {
    send_or_warn(&ipc::Op::TogglePause, "toggle-pause")
}
pub fn cancel_op() -> Result<()> {
    send_or_warn(&ipc::Op::Cancel, "cancel")
}

fn send_or_warn(op: &ipc::Op, label: &str) -> Result<()> {
    // ipc::send swallows missing-socket as Ok (fire-and-forget). We probe
    // the socket separately so we can give the user a hint when the
    // daemon's actually down.
    if !ipc::socket_path().exists() {
        eprintln!("sy knowledge: daemon socket missing — `sy knowledge {label}` had no effect");
        return Ok(());
    }
    ipc::send(op)?;
    Ok(())
}

/// Throughput / EP probe. Embeds N short strings in batches and prints
/// `chunks/s`, `mean batch ms`, `p95 batch ms`, plus the active EP. Run
/// alongside `nvidia-smi dmon -s u` to verify GPU engagement.
pub fn bench(n: usize, json_out: bool) -> Result<()> {
    let n = n.max(8);
    let pad = "lorem ipsum dolor sit amet, consectetur adipiscing elit. ";
    let texts: Vec<String> = (0..n)
        .map(|i| format!("bench chunk {i} — {}", pad.repeat(8)))
        .collect();
    let batch_size = 64usize;
    let mut batch_ms: Vec<u128> = Vec::new();
    let total_start = std::time::Instant::now();
    for chunk in texts.chunks(batch_size) {
        let t0 = std::time::Instant::now();
        let _ = embed::embed_batch(chunk)?;
        batch_ms.push(t0.elapsed().as_millis());
    }
    let total_ms = total_start.elapsed().as_millis() as f64;
    let chunks_per_s = (n as f64) * 1000.0 / total_ms.max(1.0);
    batch_ms.sort_unstable();
    let mean_ms = batch_ms.iter().sum::<u128>() as f64 / batch_ms.len().max(1) as f64;
    let p95_ms = batch_ms
        .get(((batch_ms.len() as f32) * 0.95) as usize)
        .copied()
        .unwrap_or_else(|| *batch_ms.last().unwrap_or(&0));
    let backend = embed::current_backend();
    let hardware = embed::current_hardware();
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "n": n,
                "batch_size": batch_size,
                "total_ms": total_ms,
                "chunks_per_s": chunks_per_s,
                "mean_batch_ms": mean_ms,
                "p95_batch_ms": p95_ms,
                "embed_backend": backend,
                "embed_hardware": hardware,
            }))?
        );
    } else {
        println!("embed_backend: {backend}");
        if !hardware.is_empty() {
            println!("hardware:      {hardware}");
        }
        println!("chunks:        {n}");
        println!("batch_size:    {batch_size}");
        println!("total:         {:.0} ms", total_ms);
        println!("throughput:    {:.0} chunks/s", chunks_per_s);
        println!("batch:         mean {:.1} ms, p95 {} ms", mean_ms, p95_ms);
        println!();
        println!("Tip: in another terminal, `nvidia-smi dmon -s u -c 30` polls GPU SM/MEM");
        println!("utilisation at 1 Hz — run alongside this bench to see GPU spikes.");
    }
    Ok(())
}

// ── MCP enable / disable / status ─────────────────────────────────────

/// CLI ids of the MCP detectors that are default-on (the "big four"
/// agents whose schemas we've verified). `mcp-enable` and `mcp-disable`
/// scope their auto-configure pass to these so the off-by-default
/// hint-only detectors aren't silently activated.
const MCP_DETECTOR_IDS: [&str; 5] = [
    "mcp-claude",
    "mcp-cursor",
    "mcp-codex",
    "mcp-gemini",
    "mcp-goose",
];

pub fn mcp_enable(apply: bool, json_out: bool) -> Result<()> {
    if apply {
        sources::set_mcp_enabled(true).context("set mcp_enabled=true in sy.toml")?;
    }
    let only: Vec<String> = MCP_DETECTOR_IDS.iter().map(|s| s.to_string()).collect();
    crate::auto::configure(apply, json_out, &only, &[], false)
}

pub fn mcp_disable(apply: bool, json_out: bool) -> Result<()> {
    if apply {
        sources::set_mcp_enabled(false).context("set mcp_enabled=false in sy.toml")?;
    }
    let only: Vec<String> = MCP_DETECTOR_IDS.iter().map(|s| s.to_string()).collect();
    crate::auto::configure(apply, json_out, &only, &[], false)
}

/// `sy knowledge repair-qdrant` — pre-flight scrub of the qdrant
/// storage tree. Wired as `ExecStartPre=` on `sy-qdrant.service` and
/// also called from `daemon::run()` before the in-process supervisor
/// spawns its own qdrant. Exits 0 whether or not anything was fixed
/// (idempotent). BUG-20260524-2203.
pub fn repair_qdrant(json_out: bool, quiet: bool) -> Result<()> {
    let storage = state::qdrant_storage_dir()?;
    let report = repair::quarantine_corrupt_segments(&storage)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if quiet && report.quarantined.is_empty() && report.swept_atomicwrite == 0 {
        return Ok(());
    }
    println!(
        "sy knowledge repair-qdrant: storage={} shards_scanned={} quarantined={} swept_atomicwrite={}",
        storage.display(),
        report.shards_scanned,
        report.quarantined.len(),
        report.swept_atomicwrite,
    );
    for q in &report.quarantined {
        println!(
            "  quarantined {}/{}:{} -> {}  ({})",
            q.collection,
            q.shard,
            q.segment_id,
            q.new_path.display(),
            q.reason,
        );
    }
    Ok(())
}

pub fn mcp_status_cmd(json_out: bool) -> Result<()> {
    use crate::auto_mcp;

    let rows: Vec<serde_json::Value> = auto_mcp::ALL_AGENTS
        .iter()
        .copied()
        .map(|agent| {
            let st = auto_mcp::read_state(agent);
            let (registered, command, args, path, writable) = match st {
                Some(s) => match s.registered {
                    Some(e) => (
                        true,
                        Some(e.command),
                        Some(e.args),
                        Some(s.path.display().to_string()),
                        s.writable,
                    ),
                    None => (
                        false,
                        None,
                        None,
                        Some(s.path.display().to_string()),
                        s.writable,
                    ),
                },
                None => (false, None, None, None, false),
            };
            json!({
                "agent": agent.id(),
                "label": agent.label(),
                "writable": writable,
                "path": path,
                "registered": registered,
                "command": command,
                "args": args,
            })
        })
        .collect();

    if json_out {
        let enabled = sources::mcp_enabled();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "mcp_enabled": enabled,
                "agents": rows,
            }))?
        );
        return Ok(());
    }

    println!("[knowledge].mcp_enabled = {}", sources::mcp_enabled());
    println!();
    println!("{:<14} {:<3} {:<3} PATH", "AGENT", "WR", "ON");
    for r in &rows {
        let agent = r["agent"].as_str().unwrap_or("?");
        let writable = if r["writable"].as_bool().unwrap_or(false) {
            "y"
        } else {
            "-"
        };
        let on = if r["registered"].as_bool().unwrap_or(false) {
            "y"
        } else {
            "-"
        };
        let path = r["path"].as_str().unwrap_or("(missing)");
        println!("{agent:<14} {writable:<3} {on:<3} {path}");
        if r["registered"].as_bool().unwrap_or(false) {
            let cmd = r["command"].as_str().unwrap_or("");
            let empty = Vec::new();
            let args = r["args"].as_array().unwrap_or(&empty);
            let args_s: Vec<String> = args
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect();
            println!("              command: {} {}", cmd, args_s.join(" "));
        }
    }
    Ok(())
}

/// Fuzzel-driven semantic search. Prompts for a query, runs `search`,
/// shows hits in a second fuzzel pass; selecting a hit opens the file
/// via xdg-open. Silent no-op if fuzzel isn't on PATH.
pub fn pick() -> Result<()> {
    if !crate::which("fuzzel") {
        anyhow::bail!("fuzzel not found on PATH — install fuzzel or use `sy knowledge search`");
    }
    let query = match prompt_fuzzel("🧠 search:") {
        Some(q) if !q.trim().is_empty() => q,
        _ => return Ok(()),
    };
    let hits = search_hits(&query, 12, None)?;
    if hits.is_empty() {
        crate::wifi::notify("knowledge", "(no hits)");
        return Ok(());
    }
    // Build a fuzzel menu: one row per hit. Format keeps the score and
    // a short snippet so the user can scan quickly.
    let mut rows: Vec<String> = Vec::with_capacity(hits.len());
    for h in &hits {
        let snippet = h
            .chunk_text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();
        rows.push(format!(
            "{:.3}  {}  ⟶  {}",
            h.score,
            shorten_path(&h.file_path, 60),
            snippet
        ));
    }
    let chosen = match pick_fuzzel("🧠 hits:", &rows) {
        Some(c) => c,
        None => return Ok(()),
    };
    let idx = rows.iter().position(|r| r == &chosen);
    let path = match idx {
        Some(i) => hits[i].file_path.clone(),
        None => return Ok(()),
    };
    let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
    Ok(())
}

fn prompt_fuzzel(prompt: &str) -> Option<String> {
    let mut child = std::process::Command::new("fuzzel")
        .args(["--dmenu", "--prompt", prompt, "--lines", "0"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    // No candidates — fuzzel waits for typed input.
    drop(child.stdin.take());
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn pick_fuzzel(prompt: &str, rows: &[String]) -> Option<String> {
    let mut child = std::process::Command::new("fuzzel")
        .args(["--dmenu", "--prompt", prompt, "--width", "100"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut()?;
        for r in rows {
            let _ = writeln!(stdin, "{}", r);
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn shorten_path(p: &str, max: usize) -> String {
    if p.chars().count() <= max {
        return p.to_string();
    }
    let tail: String = p
        .chars()
        .rev()
        .take(max.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

pub fn index(source: Option<&Path>, json_out: bool) -> Result<()> {
    qdrant::ensure_collection()?;
    let mut idx = state::load().unwrap_or_default();
    // Interactive `sy knowledge index` should be snappy → no throttle / cap.
    let ctx = RunCtx::interactive();
    let report = run_index(&mut idx, source, false, &ctx)?;
    idx.last_sync_unix = state::now_secs();
    state::save(&idx)?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "scanned": report.scanned,
                "indexed": report.indexed,
                "skipped": report.skipped,
                "deleted": report.deleted,
                "elapsed_ms": report.elapsed_ms,
            }))?
        );
    } else {
        println!(
            "scanned {} | indexed {} | skipped {} | deleted {} | {}ms",
            report.scanned, report.indexed, report.skipped, report.deleted, report.elapsed_ms
        );
    }
    Ok(())
}

pub fn sync(yes: bool) -> Result<()> {
    if !yes {
        anyhow::bail!(
            "this drops the Qdrant collection and re-embeds every file — re-run with --yes to confirm"
        );
    }
    // If the daemon is up, delegate to it. Running our own embedder in
    // parallel forks an extra ORT session, contends for the NPU (which
    // only services one HW context at a time), and falls back to
    // 14-thread CPU EP when it can't grab the device — turning a 2 h
    // NPU re-embed into a 30 h CPU storm.
    if status::load()
        .ok()
        .filter(|s| status::is_fresh(s) && s.daemon_running)
        .is_some()
    {
        ipc::send(&ipc::Op::FullResync).with_context(|| "send FullResync to daemon")?;
        println!("queued full resync on daemon — watch `sy knowledge status`");
        return Ok(());
    }
    qdrant::recreate_collection()?;
    let mut idx = state::Index::default();
    let ctx = RunCtx::interactive();
    let report = run_index(&mut idx, None, true, &ctx)?;
    idx.last_sync_unix = state::now_secs();
    state::save(&idx)?;
    println!(
        "full resync: indexed {} files, {} chunks, {}ms",
        report.indexed, report.chunks, report.elapsed_ms
    );
    Ok(())
}

pub fn schedule(interval: Option<&str>) -> Result<()> {
    match interval {
        None => {
            let s = sources::load()?
                .schedule
                .unwrap_or_else(|| super::DEFAULT_SCHEDULE.into());
            println!("{s}");
        }
        Some(i) => {
            sources::set_schedule(i)?;
            println!("schedule = {i}");
            let _ = ipc::send(&ipc::Op::ReloadSchedule);
        }
    }
    Ok(())
}

/// Structured-filter args for `sy knowledge search` (REQ-1/REQ-2). `kind`
/// holds kebab strings already validated by clap's `SourceKind` enum.
#[derive(Debug, Default, Clone)]
pub struct SearchArgs {
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub from: Vec<String>,
    pub kind: Vec<String>,
    pub include_source: Vec<String>,
    pub exclude_source: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn search(
    query: &str,
    limit: usize,
    json_out: bool,
    source: Option<&Path>,
    rerank: bool,
    candidates: usize,
    priority: sy_core::Priority,
    args: SearchArgs,
) -> Result<()> {
    let prefix = source.map(|p| {
        sources::expand(&p.display().to_string())
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    });
    let opts_in = include_opts_into_excluded_kinds(&args.include_source);
    let filter = build_search_filter(
        args.date_from,
        args.date_to,
        args.from,
        args.kind,
        args.include_source,
        args.exclude_source,
        opts_in,
    );
    let hits = search_hits_filtered(
        query,
        limit,
        prefix.as_deref(),
        rerank,
        candidates,
        priority,
        Some(filter),
    )?;
    if json_out {
        let arr: Vec<_> = hits
            .iter()
            .map(|h| {
                json!({
                    "score": h.score,
                    "file_path": h.file_path,
                    "chunk_index": h.chunk_index,
                    "chunk_text": h.chunk_text,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("(no hits)");
        return Ok(());
    }
    for h in &hits {
        println!("── {:.3}  {}  [{}]", h.score, h.file_path, h.chunk_index);
        for line in h.chunk_text.lines().take(4) {
            println!("  {}", line);
        }
        println!();
    }
    Ok(())
}

/// REQ-10 fetch-by-id: resolve a chunk's full (uncapped) text + payload by
/// its stable `chunk_id` over the daemon IPC. `Ok(None)` when no point
/// matches. The daemon owns the qdrant connection, so this round-trips a
/// single `Req::GetChunk` rather than opening a second client here.
pub fn get_chunk_row(chunk_id: &str) -> Result<Option<ipc::ChunkRow>> {
    let alive = status::load()
        .ok()
        .map(|s| status::is_fresh(&s) && s.daemon_running)
        .unwrap_or(false);
    if !alive {
        anyhow::bail!(
            "sy-knowledge daemon is not running — start it with \
             `systemctl --user start sy-knowledge.service`"
        );
    }
    let req = ipc::Req::GetChunk {
        chunk_id: chunk_id.to_string(),
    };
    match ipc::request_with_priority(&req, sy_core::Priority::Interactive) {
        Ok(ipc::Resp::Chunk { chunk }) => Ok(chunk),
        Ok(ipc::Resp::Error { msg }) => anyhow::bail!("daemon: {msg}"),
        Ok(other) => anyhow::bail!("daemon: unexpected response {other:?}"),
        Err(ipc::IpcError::DaemonDown) => {
            anyhow::bail!("daemon socket disappeared between liveness probe and request")
        }
        Err(ipc::IpcError::Wire(e)) => Err(e.context("ipc request")),
    }
}

/// `sy knowledge get-chunk <chunk_id>` — print the full (uncapped) chunk for
/// a stable id from a bounded search result (REQ-10).
pub fn get_chunk(chunk_id: &str, json_out: bool) -> Result<()> {
    let chunk = get_chunk_row(chunk_id)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&chunk)?);
        return Ok(());
    }
    match chunk {
        None => println!("(no chunk for id {chunk_id})"),
        Some(c) => {
            println!("chunk_id:    {}", c.chunk_id);
            println!("file_path:   {}", c.file_path);
            println!("chunk_index: {}", c.chunk_index);
            if let Some(k) = &c.kind {
                println!("kind:        {k}");
            }
            if let Some(s) = &c.source_name {
                println!("source_name: {s}");
            }
            println!();
            println!("{}", c.text);
        }
    }
    Ok(())
}

/// Kind that `knowledge_search` excludes from default scope (REQ-1): the
/// agent's own Claude transcripts must never poison a fresh lookup unless
/// the caller explicitly opts that kind back in.
/// Source kinds excluded from default search scope: an agent's own outputs,
/// which must never surface as default evidence (self-poisoning). Both are
/// still searchable on explicit opt-in. `claude-transcripts` is the legacy
/// REQ-1 case; `agent-history` covers every agent dotfile home.
pub const DEFAULT_EXCLUDED_KINDS: &[&str] = &["claude-transcripts", "agent-history"];

/// Compile the additive search-filter args (CLI flags / MCP params) into a
/// [`ipc::SearchFilter`], applying the default-exclude of every
/// [`DEFAULT_EXCLUDED_KINDS`] entry. A given kind's exclusion is dropped when
/// the caller opts it back in — either by naming it in `kind`, or via an
/// `--include-source` that resolves to a source of that kind
/// (`include_opts_in_kinds`, decided against the registry by the caller).
#[allow(clippy::too_many_arguments)]
pub fn build_search_filter(
    date_from: Option<String>,
    date_to: Option<String>,
    from: Vec<String>,
    kind: Vec<String>,
    include_sources: Vec<String>,
    exclude_sources: Vec<String>,
    include_opts_in_kinds: Vec<String>,
) -> ipc::SearchFilter {
    let exclude_kinds = DEFAULT_EXCLUDED_KINDS
        .iter()
        .filter(|dk| {
            // Keep excluding this kind unless the caller opted it back in.
            !kind.iter().any(|k| k == *dk) && !include_opts_in_kinds.iter().any(|k| k == *dk)
        })
        .map(|dk| (*dk).to_string())
        .collect();
    ipc::SearchFilter {
        date_from,
        date_to,
        from,
        kind,
        include_sources,
        exclude_sources,
        exclude_kinds,
    }
}

/// The subset of [`DEFAULT_EXCLUDED_KINDS`] that `include_sources` opts back
/// into scope — i.e. each default-excluded kind that a named source resolves
/// to. Returns empty when the registry can't be read so the default-exclude
/// stays in force (fail-safe for REQ-1).
pub fn include_opts_into_excluded_kinds(include_sources: &[String]) -> Vec<String> {
    if include_sources.is_empty() {
        return Vec::new();
    }
    let Ok(section) = sources::load() else {
        return Vec::new();
    };
    DEFAULT_EXCLUDED_KINDS
        .iter()
        .filter(|dk| {
            section
                .sources
                .iter()
                .any(|s| s.kind.as_kebab() == **dk && include_sources.iter().any(|n| n == &s.name))
        })
        .map(|dk| (*dk).to_string())
        .collect()
}

/// Exit code for `sy knowledge eval` when a metric regresses past
/// tolerance (CLAUDE.md exit-code convention: 3 = drift detected). CI
/// gates `make eval` on this non-zero exit.
pub const EVAL_DRIFT: i32 = 3;

/// Default CI regression floors for `sy knowledge eval` (REQ-9). Tuned
/// conservatively against a tiny golden set; raise as recall improves.
pub const DEFAULT_EVAL_TOLERANCE: eval::Tolerance = eval::Tolerance {
    min_recall_at_1: 0.3,
    min_recall_at_5: 0.5,
    min_mrr: 0.4,
    min_abstain_accuracy: 0.5,
};

/// Repo-relative location of the checked-in golden set (REQ-9).
pub const GOLDEN_SET_REL: &str = "specs/knowledge-feedback-iter1/eval/queries.jsonl";

/// Parse a `queries.jsonl` body into labelled rows (blank lines skipped).
pub fn parse_golden_set(body: &str) -> Result<Vec<eval::LabelledQuery>> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<eval::LabelledQuery>(l).context("parse golden-set row"))
        .collect()
}

/// Pure eval core (hermetic, no daemon): run each labelled query through
/// `runner`, compute [`eval::metrics`], print them (human or `--json`),
/// and return a drift error when any metric falls below `tol`. The live
/// `sy knowledge eval` passes a `runner` backed by the daemon search
/// path; unit tests inject fixture rankings so the JSON emission and the
/// exit-code logic are tested without a live qdrant/daemon.
pub fn run_eval<R>(
    queries: &[eval::LabelledQuery],
    runner: R,
    json_out: bool,
    tol: &eval::Tolerance,
) -> Result<()>
where
    R: Fn(&eval::LabelledQuery) -> Result<eval::RankedResult>,
{
    let ranked: Vec<eval::RankedResult> = queries.iter().map(&runner).collect::<Result<_>>()?;
    let m = eval::metrics(queries, &ranked);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&m)?);
    } else {
        println!("recall@1         {:.3}", m.recall_at_1);
        println!("recall@5         {:.3}", m.recall_at_5);
        println!("mrr              {:.3}", m.mrr);
        println!("abstain_accuracy {:.3}", m.abstain_accuracy);
        println!("n                {}", m.n);
    }
    if let Some(reason) = tol.regression(&m) {
        return Err(super::KnowledgeError {
            code: EVAL_DRIFT,
            msg: format!("eval regression: {reason}"),
        }
        .into());
    }
    Ok(())
}

/// `sy knowledge eval [--json]` — load the checked-in golden set, run
/// each query through the live daemon search path, report recall@1/5,
/// MRR, and abstain accuracy, and exit non-zero on regression (REQ-9).
pub fn eval_cmd(json_out: bool) -> Result<()> {
    let path = repo_relative(GOLDEN_SET_REL)?;
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("read golden set {}", path.display()))?;
    let queries = parse_golden_set(&body)?;
    run_eval(&queries, run_query_live, json_out, &DEFAULT_EVAL_TOLERANCE)
}

/// Run one labelled query through the live daemon search path and reduce
/// the response to a [`eval::RankedResult`] (hit text + abstain flag).
fn run_query_live(q: &eval::LabelledQuery) -> Result<eval::RankedResult> {
    let filter = build_search_filter(
        q.date_from.clone(),
        q.date_to.clone(),
        Vec::new(),
        q.kind.clone().into_iter().collect(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let outcome = search_outcome_filtered(
        &q.query,
        eval::RECALL_K,
        None,
        true,
        8,
        sy_core::Priority::Interactive,
        Some(filter),
        Some(DEFAULT_EVAL_ABSTAIN),
    )?;
    Ok(eval::RankedResult {
        ids: outcome
            .hits
            .iter()
            .map(|h| format!("{}#{}\n{}", h.file_path, h.chunk_index, h.chunk_text))
            .collect(),
        abstained: outcome.abstained,
    })
}

/// Abstain threshold used by the eval runner so unanswerable golden-set
/// rows can register as true-negatives (REQ-6 calibration boundary).
const DEFAULT_EVAL_ABSTAIN: f32 = 0.5;

/// Resolve a repo-relative path. Prefers `$SY_ROOT`, else the compiled-in
/// `CARGO_MANIFEST_DIR` (matches the policy resolver's convention).
fn repo_relative(rel: &str) -> Result<PathBuf> {
    let root = std::env::var_os("SY_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    Ok(root.join(rel))
}

/// Shared search path. The daemon is the only process that owns the
/// NPU — used by `sy knowledge search`, `sy knowledge pick`, and the
/// embedded MCP server's `tool_search`. Returns a hard error if the
/// daemon isn't reachable; there is no in-process fallback (that
/// path would race the daemon for /dev/accel/accel0 and end up on
/// CPU silently).
///
/// Default flow is two-stage (embed → qdrant top-N → bge-reranker).
/// See `search_hits_opts` for the flag-aware version.
pub fn search_hits(query: &str, limit: usize, prefix: Option<&str>) -> Result<Vec<ipc::HitRow>> {
    search_hits_opts(
        query,
        limit,
        prefix,
        true,
        30,
        sy_core::Priority::Interactive,
    )
}

/// Flag-aware search. `rerank=true` (default) does embed → qdrant
/// top-`candidates` → bge-reranker → top-`limit`. `rerank=false`
/// skips the cross-encoder pass for lower latency. `priority`
/// controls the scheduler class for the embed pass (SPEC §4.7);
/// CLI surfaces default `Interactive`, daemon-internal callers
/// override to `Background` for indexing passes.
pub fn search_hits_opts(
    query: &str,
    limit: usize,
    prefix: Option<&str>,
    rerank: bool,
    candidates: usize,
    priority: sy_core::Priority,
) -> Result<Vec<ipc::HitRow>> {
    search_hits_filtered(query, limit, prefix, rerank, candidates, priority, None)
}

/// Full search response: ranked hits plus the REQ-6 calibrated
/// `confidence` and `abstained` flag. The MCP `tool_search` surfaces
/// these; the plain [`search_hits_filtered`] discards them for callers
/// that only want the ranked list.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub hits: Vec<ipc::HitRow>,
    pub confidence: f32,
    pub abstained: bool,
}

/// Like [`search_hits_opts`] but carries a compiled [`ipc::SearchFilter`]
/// (REQ-1/REQ-2) and surfaces the REQ-6 confidence/abstain envelope. An
/// `abstain_threshold` of `None` keeps the daemon's server default (no
/// abstain). `filter: None` reproduces the unfiltered legacy path.
#[allow(clippy::too_many_arguments)]
pub fn search_outcome_filtered(
    query: &str,
    limit: usize,
    prefix: Option<&str>,
    rerank: bool,
    candidates: usize,
    priority: sy_core::Priority,
    filter: Option<ipc::SearchFilter>,
    abstain_threshold: Option<f32>,
) -> Result<SearchOutcome> {
    let alive = status::load()
        .ok()
        .map(|s| status::is_fresh(&s) && s.daemon_running)
        .unwrap_or(false);
    if !alive {
        anyhow::bail!(
            "sy-knowledge daemon is not running — start it with \
             `systemctl --user start sy-knowledge.service` \
             (or `sy knowledge install-service` for first-time setup)"
        );
    }
    let req = if rerank {
        ipc::Req::SearchRerank {
            query: query.to_string(),
            limit,
            prefix: prefix.map(String::from),
            candidates,
            priority,
            filter,
            abstain_threshold,
        }
    } else {
        ipc::Req::Search {
            query: query.to_string(),
            limit,
            prefix: prefix.map(String::from),
            priority,
            filter,
            abstain_threshold,
        }
    };
    match ipc::request_with_priority(&req, priority) {
        Ok(ipc::Resp::Search {
            hits,
            confidence,
            abstained,
        }) => Ok(SearchOutcome {
            hits,
            confidence,
            abstained,
        }),
        Ok(ipc::Resp::Error { msg }) => anyhow::bail!("daemon: {msg}"),
        Ok(other) => anyhow::bail!("daemon: unexpected response {other:?}"),
        Err(ipc::IpcError::DaemonDown) => {
            anyhow::bail!("daemon socket disappeared between liveness probe and request")
        }
        Err(ipc::IpcError::Wire(e)) => Err(e.context("ipc request")),
    }
}

/// Like [`search_outcome_filtered`] but discards the confidence envelope,
/// returning just the ranked hits (REQ-1/REQ-2). `filter: None`
/// reproduces the unfiltered legacy path.
#[allow(clippy::too_many_arguments)]
pub fn search_hits_filtered(
    query: &str,
    limit: usize,
    prefix: Option<&str>,
    rerank: bool,
    candidates: usize,
    priority: sy_core::Priority,
    filter: Option<ipc::SearchFilter>,
) -> Result<Vec<ipc::HitRow>> {
    search_outcome_filtered(
        query, limit, prefix, rerank, candidates, priority, filter, None,
    )
    .map(|o| o.hits)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct IndexReport {
    pub scanned: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub deleted: usize,
    pub chunks: usize,
    pub elapsed_ms: u128,
}

/// One indexable folder + the rules that govern it. Built up from explicit
/// `[[knowledge.sources]]` entries (mode = explicit) plus every active
/// `qdr.toml` manifest under shallow-`$HOME` and the registered discover
/// roots. The walk + chunk + embed pipeline below treats jobs uniformly.
struct IndexJob {
    /// Absolute folder root (matches what we stamp into `payload.source`).
    folder: PathBuf,
    /// Storage-form source label used by `--source` filtering and stale
    /// cleanup. For explicit sources this is the original sy.toml entry;
    /// for manifests it's the folder path expanded.
    source_tag: String,
    /// Drives per-kind pipeline selection (`pipeline::select`). Defaults to
    /// `Generic`, which preserves the historical chunk-and-embed behaviour.
    kind: SourceKind,
    walker: WalkBuilder,
    glob_filter: Option<manifest::ManifestGlobFilter>,
    max_file_bytes: u64,
    tags: Vec<String>,
}

/// Resolve the effective [`SourceKind`] for a single file being indexed.
/// The job's `kind` is computed once from its ROOT, but a manifest root
/// (e.g. `~/.claude`) can classify Generic while individual files under it
/// (e.g. `~/.claude/projects/**/*.jsonl`) are transcripts. Classify the
/// FILE's full path; fall back to the job kind only when the file path
/// itself is unclassifiable (Generic). This drives both pipeline selection
/// and the payload `kind` stamp so REQ-1 default-exclusion fires per file.
fn effective_kind(file_path: &str, job_kind: SourceKind) -> SourceKind {
    match sources::classify_kind(file_path) {
        SourceKind::Generic => job_kind,
        specific => specific,
    }
}

fn explicit_job(root: &Path) -> IndexJob {
    let mut wb = WalkBuilder::new(root);
    wb.hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true);
    let job_root = root.to_path_buf();
    wb.filter_entry(move |dent| {
        if !dent.file_type().is_some_and(|ft| ft.is_dir()) {
            return true;
        }
        if dent.path() == job_root.as_path() {
            return true;
        }
        // A nested directory with its own qdr.toml is owned by an inner
        // manifest job — don't double-index from the explicit-mode walk.
        !dent.path().join(manifest::MANIFEST_FILENAME).exists()
    });
    IndexJob {
        folder: root.to_path_buf(),
        source_tag: root.display().to_string(),
        kind: sources::kind_for_path(root),
        walker: wb,
        glob_filter: None,
        max_file_bytes: extract::DEFAULT_MAX_BYTES,
        tags: Vec::new(),
    }
}

fn manifest_job(m: &manifest::QdrManifest) -> Result<IndexJob> {
    Ok(IndexJob {
        folder: m.folder.clone(),
        source_tag: m.folder.display().to_string(),
        kind: sources::kind_for_path(&m.folder),
        walker: m.walker(),
        glob_filter: m.glob_filter()?,
        max_file_bytes: m.max_file_bytes,
        tags: m.tags.clone(),
    })
}

/// Build every job that should run this pass. `only_source` short-circuits
/// to a single explicit job; otherwise we collect explicit sources +
/// enabled manifests.
fn collect_jobs(only_source: Option<&Path>) -> Result<Vec<IndexJob>> {
    if let Some(s) = only_source {
        let root = sources::expand(&s.display().to_string())?;
        // If `--source` matches a known manifest folder, prefer the
        // manifest job so include/exclude/tags apply.
        for m in manifest::discover_all() {
            if m.folder == root && m.enabled {
                return Ok(vec![manifest_job(&m)?]);
            }
        }
        return Ok(vec![explicit_job(&root)]);
    }
    let mut jobs = Vec::new();
    for root in sources::enabled_paths()? {
        if !root.exists() {
            eprintln!("sy knowledge: source missing: {}", root.display());
            continue;
        }
        jobs.push(explicit_job(&root));
    }
    for m in manifest::discover_all() {
        if !m.enabled {
            continue;
        }
        match manifest_job(&m) {
            Ok(j) => jobs.push(j),
            Err(e) => eprintln!("sy knowledge: skip manifest {}: {e}", m.folder.display()),
        }
    }
    Ok(jobs)
}

/// Walk source roots, embed/upsert new+changed files, drop deleted ones.
/// Public so the daemon can reuse it. `ctx` carries the cancellation
/// token, the cooperative throttle, and the adaptive CPU cap (if any).
/// Interactive CLI callers pass `RunCtx::interactive()`; the daemon
/// passes `RunCtx::for_daemon_pass(...)`.
pub fn run_index(
    idx: &mut state::Index,
    only_source: Option<&Path>,
    full_resync: bool,
    ctx: &RunCtx,
) -> Result<IndexReport> {
    let start = std::time::Instant::now();
    let jobs = collect_jobs(only_source)?;
    if jobs.is_empty() {
        return Ok(IndexReport::default());
    }

    let mut report = IndexReport::default();
    let mut seen_files: HashSet<String> = HashSet::new();
    let mut pending_files: Vec<PendingFile> = Vec::new();

    'outer: for job in &jobs {
        if !job.folder.exists() {
            eprintln!("sy knowledge: source missing: {}", job.folder.display());
            continue;
        }
        for dent in job.walker.clone().build() {
            if ctx.cancelled() {
                break 'outer;
            }
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue,
            };
            let p = dent.path();
            if !p.is_file() {
                continue;
            }
            // Skip the manifest file itself — embedding the marker is noise.
            if p.file_name().and_then(|n| n.to_str()) == Some(manifest::MANIFEST_FILENAME) {
                continue;
            }
            if let Some(filter) = &job.glob_filter {
                if !filter.matches(p) {
                    continue;
                }
            }
            report.scanned += 1;
            let key = p.display().to_string();
            seen_files.insert(key.clone());

            let mtime = state::mtime_secs(p);
            let unchanged = if full_resync {
                false
            } else {
                idx.files
                    .get(&key)
                    .map(|e| e.mtime == mtime)
                    .unwrap_or(false)
            };
            if unchanged {
                continue;
            }

            let text = match extract::extract_with_limit(p, job.max_file_bytes)? {
                extract::Extracted::Text(t) => t,
                extract::Extracted::Skip(reason) => {
                    if matches!(
                        reason,
                        extract::SkipReason::PdfToTextMissing
                            | extract::SkipReason::PdfFailed(_)
                            | extract::SkipReason::ReadFailed(_)
                    ) {
                        match reason.detail() {
                            Some(detail) => eprintln!(
                                "sy knowledge: skip {} ({}: {})",
                                p.display(),
                                reason.label(),
                                detail
                            ),
                            None => {
                                eprintln!("sy knowledge: skip {} ({})", p.display(), reason.label())
                            }
                        }
                    }
                    report.skipped += 1;
                    continue;
                }
            };
            let hash = state::hash_bytes(text.as_bytes());
            if !full_resync {
                if let Some(e) = idx.files.get(&key) {
                    if e.content_hash == hash && e.mtime == mtime {
                        continue;
                    }
                }
            }

            // Classify per FILE, not per job root: a `~/.claude` manifest
            // root is Generic but its `projects/**/*.jsonl` files are
            // transcripts (REQ-1). This drives pipeline selection AND the
            // payload `kind` stamp so default-exclusion fires per file.
            let file_kind = effective_kind(&key, job.kind);
            let mut records = pipeline::select(file_kind).records(&key, &text);
            // Telegram voice notes / round videos are transcribed (cached
            // content-addressed next to the media) into kind=telegram-voice
            // chunks pointing at the source media. Already-transcribed media
            // short-circuits; a disabled backend (no `transcribe` feature)
            // emits nothing. Runs inside this scan loop so it honours the
            // same cancellation check as the rest of the pass.
            if file_kind == SourceKind::Telegram {
                let tx = transcribe::default_transcriber();
                records.extend(pipeline::telegram::TelegramPipeline.voice_records(
                    &key,
                    &text,
                    tx.as_ref(),
                ));
            }
            if records.is_empty() {
                report.skipped += 1;
                continue;
            }
            if !full_resync {
                if let Some(e) = idx.files.remove(&key) {
                    qdrant::delete_points(&e.point_ids)?;
                }
            }
            pending_files.push(PendingFile {
                path: p.to_path_buf(),
                key,
                hash,
                records,
                source_tag: job.source_tag.clone(),
                tags: job.tags.clone(),
                kind: file_kind,
            });
        }
    }

    // Embed in batches; track point ids per file as we go.
    let mut batch_texts: Vec<String> = Vec::with_capacity(UPSERT_BATCH);
    let mut batch_meta: Vec<(usize, usize)> = Vec::with_capacity(UPSERT_BATCH);
    let mut file_point_ids: Vec<Vec<String>> = vec![Vec::new(); pending_files.len()];

    'embed: for (fi, item) in pending_files.iter().enumerate() {
        if ctx.cancelled() {
            break 'embed;
        }
        let records = &item.records;
        for (ci, r) in records.iter().enumerate() {
            batch_texts.push(r.text.clone());
            batch_meta.push((fi, ci));
            if batch_texts.len() >= UPSERT_BATCH {
                flush_batch(
                    &mut batch_texts,
                    &mut batch_meta,
                    &pending_files,
                    &mut file_point_ids,
                    &mut report,
                    ctx,
                )?;
                if ctx.cancelled() {
                    break 'embed;
                }
            }
        }
    }
    flush_batch(
        &mut batch_texts,
        &mut batch_meta,
        &pending_files,
        &mut file_point_ids,
        &mut report,
        ctx,
    )?;

    for (i, item) in pending_files.into_iter().enumerate() {
        // Only commit files whose chunks all made it into qdrant. After a
        // mid-pass cancel, late-pending files have empty point_ids vecs —
        // skip them so the next pass treats them as still-changed.
        if file_point_ids[i].is_empty() {
            continue;
        }
        idx.files.insert(
            item.key,
            state::FileEntry {
                mtime: state::mtime_secs(&item.path),
                content_hash: item.hash,
                point_ids: std::mem::take(&mut file_point_ids[i]),
            },
        );
        report.indexed += 1;
    }

    // Stale-cleanup is risky after a cancel — we may not have walked every
    // source, so files that look "missing" might just be unwalked. Skip it.
    if only_source.is_none() && !ctx.cancelled() {
        let stale: Vec<String> = idx
            .files
            .keys()
            .filter(|k| !seen_files.contains(*k))
            .cloned()
            .collect();
        for k in stale {
            if let Some(e) = idx.files.remove(&k) {
                let _ = qdrant::delete_points(&e.point_ids);
                report.deleted += 1;
            }
        }
    }

    report.elapsed_ms = start.elapsed().as_millis();
    Ok(report)
}

/// Per-file batch entry queued by `index_jobs` and consumed by
/// `flush_batch`. Module-scope so both functions share the layout;
/// not exposed outside this module because it carries internal
/// chunking detail no other caller needs.
struct PendingFile {
    path: PathBuf,
    key: String,
    hash: String,
    records: Vec<Record>,
    source_tag: String,
    tags: Vec<String>,
    /// Source kind, stamped onto every point as the kebab `kind` payload so
    /// the default-scope filter (Step 11) can exclude `claude-transcripts`.
    kind: SourceKind,
}

fn flush_batch(
    texts: &mut Vec<String>,
    meta: &mut Vec<(usize, usize)>,
    pending: &[PendingFile],
    file_point_ids: &mut [Vec<String>],
    report: &mut IndexReport,
    ctx: &RunCtx,
) -> Result<()> {
    if texts.is_empty() {
        return Ok(());
    }
    if ctx.cancelled() {
        // Drop the pending batch — caller will partial-commit.
        texts.clear();
        meta.clear();
        return Ok(());
    }
    let vectors = embed::embed_batch(texts)?;
    let mut points = Vec::with_capacity(vectors.len());
    for (i, vec) in vectors.into_iter().enumerate() {
        let (fi, ci) = meta[i];
        let item = &pending[fi];
        let record = &item.records[ci];
        let id = record.chunk_id.clone();
        file_point_ids[fi].push(id.clone());
        points.push(build_point(id, vec, record, item));
        report.chunks += 1;
    }
    qdrant::upsert(&points)?;
    texts.clear();
    meta.clear();
    ctx.after_batch();
    Ok(())
}

/// Build an upsert `Point` for one chunk: the named `dense` embedding plus the
/// in-house term-frequency `sparse` vector (Step 4). Pure so the dual-vector
/// construction is unit-testable without a daemon or live qdrant.
fn build_point(id: String, vector: Vec<f32>, record: &Record, item: &PendingFile) -> Point {
    Point {
        id,
        vector,
        sparse: sparse::encode(&record.text),
        payload: PointPayload {
            source: item.source_tag.clone(),
            file_path: record
                .payload
                .file_path
                .clone()
                .unwrap_or_else(|| item.key.clone()),
            chunk_index: record.payload.chunk_index,
            chunk_text: record.text.clone(),
            file_mtime: state::mtime_secs(&item.path),
            content_hash: item.hash.clone(),
            tags: item.tags.clone(),
            kind: Some(
                record
                    .payload
                    .kind
                    .clone()
                    .unwrap_or_else(|| item.kind.as_kebab().to_string()),
            ),
            date: record.payload.date.clone(),
            from: record.payload.from.clone(),
            message_id: record.payload.message_id,
            reply_to_id: record.payload.reply_to_id,
            has_media: record.payload.has_media,
            model: record.payload.model.clone(),
            project_id: record.payload.project_id.clone(),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_point_carries_dense_and_sparse() {
        let item = PendingFile {
            path: PathBuf::from("/tmp/does-not-exist.txt"),
            key: "src/doc.txt".into(),
            hash: "h".into(),
            records: vec![],
            source_tag: "s".into(),
            tags: vec![],
            kind: SourceKind::Generic,
        };
        let record = Record {
            chunk_id: super::super::chunk::point_id(&item.key, 0),
            payload: pipeline::RecordPayload {
                chunk_index: 0,
                ..Default::default()
            },
            // A rare literal token must survive into the sparse vector.
            text: "новый год X5 Магнит".into(),
        };
        let point = build_point(record.chunk_id.clone(), vec![0.1, 0.2, 0.3], &record, &item);

        // Sparse vector is non-empty and well-formed.
        assert!(!point.sparse.indices.is_empty());
        assert_eq!(point.sparse.indices.len(), point.sparse.values.len());

        // The flushed point serializes BOTH named vectors on the wire.
        let body = qdrant::upsert_body(std::slice::from_ref(&point));
        let v = &body["points"][0]["vector"];
        assert_eq!(v["dense"].as_array().expect("dense array").len(), 3);
        assert!(v["sparse"]["indices"].is_array());
        assert!(!v["sparse"]["indices"].as_array().expect("array").is_empty());
        assert!(v["sparse"]["values"].is_array());
    }

    #[test]
    fn search_args_compile_to_searchfilter() {
        // All filter args populate the matching SearchFilter fields, and a
        // search with no opt-in default-excludes EVERY self-poisoning kind
        // (`claude-transcripts` + `agent-history`).
        let f = build_search_filter(
            Some("2024-01-01T00:00:00Z".into()),
            Some("2024-12-31T23:59:59Z".into()),
            vec!["alice".into()],
            vec!["telegram".into()],
            vec!["tg-main".into()],
            vec!["spam".into()],
            Vec::new(),
        );
        assert_eq!(f.date_from.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(f.date_to.as_deref(), Some("2024-12-31T23:59:59Z"));
        assert_eq!(f.from, vec!["alice".to_string()]);
        assert_eq!(f.kind, vec!["telegram".to_string()]);
        assert_eq!(f.include_sources, vec!["tg-main".to_string()]);
        assert_eq!(f.exclude_sources, vec!["spam".to_string()]);
        for dk in DEFAULT_EXCLUDED_KINDS {
            assert!(f.exclude_kinds.contains(&dk.to_string()), "missing {dk}");
        }
    }

    #[test]
    fn explicit_kind_opt_in_drops_only_that_default_exclude() {
        // Naming `agent-history` in `kind` lifts its exclusion but leaves the
        // other default-excluded kinds (claude-transcripts) in force.
        let f = build_search_filter(
            None,
            None,
            vec![],
            vec!["agent-history".into()],
            vec![],
            vec![],
            Vec::new(),
        );
        assert!(!f.exclude_kinds.contains(&"agent-history".to_string()));
        assert!(f.exclude_kinds.contains(&"claude-transcripts".to_string()));
    }

    #[test]
    fn include_source_opt_in_drops_only_that_kind() {
        // An include-source resolving to a claude-transcripts source lifts
        // that kind's exclusion; agent-history stays excluded.
        let f = build_search_filter(
            None,
            None,
            vec![],
            vec![],
            vec!["proj".into()],
            vec![],
            vec!["claude-transcripts".into()],
        );
        assert!(!f.exclude_kinds.contains(&"claude-transcripts".to_string()));
        assert!(f.exclude_kinds.contains(&"agent-history".to_string()));
    }

    #[test]
    fn transcript_point_carries_claude_transcripts_kind_and_date() {
        let item = PendingFile {
            path: PathBuf::from("/tmp/does-not-exist.jsonl"),
            key: "~/.claude/projects/proj/s.jsonl".into(),
            hash: "h".into(),
            records: vec![],
            source_tag: "s".into(),
            tags: vec![],
            kind: SourceKind::ClaudeTranscripts,
        };
        let record = Record {
            chunk_id: super::super::chunk::point_id(&item.key, 0),
            payload: pipeline::RecordPayload {
                chunk_index: 0,
                date: Some("2024-01-01T10:00:00Z".into()),
                ..Default::default()
            },
            text: "hello from claude".into(),
        };
        let point = build_point(record.chunk_id.clone(), vec![0.1, 0.2, 0.3], &record, &item);
        assert_eq!(point.payload.kind.as_deref(), Some("claude-transcripts"));
        assert_eq!(point.payload.date.as_deref(), Some("2024-01-01T10:00:00Z"));
    }

    #[test]
    fn transcript_file_under_generic_job_resolves_to_transcripts_kind() {
        // REQ-1 regression: a `~/.claude/projects/**/*.jsonl` file indexed
        // under a manifest job whose ROOT (`~/.claude`) classified as
        // Generic must still resolve to ClaudeTranscripts from its OWN path.
        let k = effective_kind(
            "/home/dmitriy/.claude/projects/abc/sess.jsonl",
            SourceKind::Generic,
        );
        assert_eq!(k, SourceKind::ClaudeTranscripts);
    }

    #[test]
    fn non_projects_file_under_claude_is_agent_history() {
        // A non-transcript file under `.claude` is the agent's own dotfile
        // content → AgentHistory (default-excluded), not a spurious
        // claude-transcripts stamp and not Generic.
        let k = effective_kind("/home/dmitriy/.claude/notes/todo.md", SourceKind::Generic);
        assert_eq!(k, SourceKind::AgentHistory);
    }

    #[test]
    fn md_sibling_outside_agent_homes_stays_generic() {
        // A plain `.md` under a real personal Generic job stays generic.
        let k = effective_kind("/home/dmitriy/knowledge/notes/todo.md", SourceKind::Generic);
        assert_eq!(k, SourceKind::Generic);
    }

    #[test]
    fn generic_file_inherits_non_generic_job_kind() {
        // A file the classifier can't peg (Generic) inherits the job kind,
        // so an explicit telegram job still stamps telegram on its files.
        let k = effective_kind("/home/dmitriy/knowledge/notes/x.txt", SourceKind::Telegram);
        assert_eq!(k, SourceKind::Telegram);
    }

    #[test]
    fn transcript_file_payload_kind_under_generic_job() {
        // End-to-end at the point layer: a transcript file's point carries
        // kind=claude-transcripts even when the PendingFile.kind is Generic
        // (mixed-tree manifest job), and a generic sibling does not.
        let tx_item = PendingFile {
            path: PathBuf::from("/tmp/does-not-exist.jsonl"),
            key: "/home/dmitriy/.claude/projects/abc/sess.jsonl".into(),
            hash: "h".into(),
            records: vec![],
            source_tag: "agent-claude".into(),
            tags: vec![],
            kind: effective_kind(
                "/home/dmitriy/.claude/projects/abc/sess.jsonl",
                SourceKind::Generic,
            ),
        };
        let record = Record {
            chunk_id: super::super::chunk::point_id(&tx_item.key, 0),
            payload: pipeline::RecordPayload {
                chunk_index: 0,
                ..Default::default()
            },
            text: "secret session".into(),
        };
        let point = build_point(
            record.chunk_id.clone(),
            vec![0.1, 0.2, 0.3],
            &record,
            &tx_item,
        );
        assert_eq!(point.payload.kind.as_deref(), Some("claude-transcripts"));

        let md_item = PendingFile {
            path: PathBuf::from("/tmp/does-not-exist.md"),
            key: "/home/dmitriy/.claude/notes/todo.md".into(),
            hash: "h".into(),
            records: vec![],
            source_tag: "agent-claude".into(),
            tags: vec![],
            kind: effective_kind("/home/dmitriy/.claude/notes/todo.md", SourceKind::Generic),
        };
        let md_record = Record {
            chunk_id: super::super::chunk::point_id(&md_item.key, 0),
            payload: pipeline::RecordPayload {
                chunk_index: 0,
                ..Default::default()
            },
            text: "just a note".into(),
        };
        let md_point = build_point(
            md_record.chunk_id.clone(),
            vec![0.1, 0.2, 0.3],
            &md_record,
            &md_item,
        );
        assert_ne!(md_point.payload.kind.as_deref(), Some("claude-transcripts"));
    }

    fn golden(query: &str, expected: &str, answerable: bool) -> eval::LabelledQuery {
        serde_json::from_value(json!({
            "query": query, "expected": expected, "answerable": answerable
        }))
        .expect("labelled query")
    }

    /// The injectable runner seam lets us drive `run_eval` with fixture
    /// rankings — no daemon/qdrant — so the JSON metric emission is
    /// pinned hermetically (the live path is the integration use).
    #[test]
    fn eval_cmd_emits_json_metrics() {
        let queries = vec![golden("найди X5", "X5 Магнит", true)];
        let runner = |_q: &eval::LabelledQuery| {
            Ok(eval::RankedResult {
                ids: vec!["chunk about X5 Магнит".into()],
                abstained: false,
            })
        };
        // Loose tolerance so a perfect run does not regress.
        let tol = eval::Tolerance {
            min_recall_at_1: 0.0,
            min_recall_at_5: 0.0,
            min_mrr: 0.0,
            min_abstain_accuracy: 0.0,
        };
        // Drives the metrics + JSON branch; returns Ok (no regression).
        run_eval(&queries, runner, true, &tol).expect("json metrics, no regression");
    }

    #[test]
    fn eval_returns_nonzero_on_regression_past_tolerance() {
        // Answerable query whose gold never surfaces → recall 0 → below
        // any positive floor → drift exit code 3.
        let queries = vec![golden("найди X5", "X5 Магнит", true)];
        let runner = |_q: &eval::LabelledQuery| {
            Ok(eval::RankedResult {
                ids: vec!["unrelated noise".into()],
                abstained: false,
            })
        };
        let err =
            run_eval(&queries, runner, true, &DEFAULT_EVAL_TOLERANCE).expect_err("must regress");
        let ke = err
            .downcast_ref::<super::super::KnowledgeError>()
            .expect("KnowledgeError");
        assert_eq!(ke.code, EVAL_DRIFT);
    }

    #[test]
    fn checked_in_golden_set_parses_with_required_categories() {
        // The shipped queries.jsonl deserializes (extra bookkeeping
        // fields tolerated) and carries the required category counts.
        let path = repo_relative(GOLDEN_SET_REL).expect("repo path");
        let body = std::fs::read_to_string(&path).expect("read golden set");
        let queries = parse_golden_set(&body).expect("parse golden set");
        assert!((20..=40).contains(&queries.len()), "20-40 rows");
        assert!(queries.iter().filter(|q| !q.answerable).count() >= 5);
    }
}
