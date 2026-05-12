//! `sy knowledge {add|rm|list|index|sync|schedule|search}` impls.
//!
//! Most commands are pure functions of disk state (sy.toml + index.json +
//! Qdrant). They work without the daemon running. After mutating sy.toml
//! they fire a non-blocking IPC notification to the daemon if it's up.

use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde_json::json;

use super::{
    chunk, embed, exit, extract, ipc, manifest,
    qdrant::{self, Point, PointPayload},
    runctx::RunCtx,
    sources::{self, SourceMode},
    state, status,
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
        println!("{:<3} {:<8} {}", "EN", "MODE", "PATH");
        for s in &section.sources {
            let resolved = sources::expand(&s.path).unwrap_or_else(|_| PathBuf::from(&s.path));
            let mark = if s.enabled { "y" } else { "-" };
            let mode = match s.mode {
                SourceMode::Explicit => "explicit",
                SourceMode::Discover => "discover",
            };
            println!("{:<3} {:<8} {}  ({})", mark, mode, s.path, resolved.display());
        }
    }
    if !discovered.is_empty() {
        println!();
        println!("discovered ({} qdr.toml manifest{})", discovered.len(), if discovered.len() == 1 { "" } else { "s" });
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
        if i > 0 && (bytes.len() - i) % 3 == 0 {
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
        format!("{} ago", human_secs(now.saturating_sub(s.last_index_at_unix)))
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
    tags.sort_by(|a, b| b.1.cmp(&a.1));

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
                println!("{}", serde_json::to_string_pretty(&json!({"daemon_running": false}))?);
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
        eprintln!(
            "sy knowledge: daemon socket missing — `sy knowledge {label}` had no effect"
        );
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
        let _ = embed::embed_batch(&chunk.iter().cloned().collect::<Vec<_>>())?;
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
        println!(
            "Tip: in another terminal, `nvidia-smi dmon -s u -c 30` polls GPU SM/MEM"
        );
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

    println!(
        "[knowledge].mcp_enabled = {}",
        sources::mcp_enabled()
    );
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
    if status::load().ok().filter(|s| status::is_fresh(s) && s.daemon_running).is_some() {
        ipc::send(&ipc::Op::FullResync)
            .with_context(|| "send FullResync to daemon")?;
        println!(
            "queued full resync on daemon — watch `sy knowledge status`"
        );
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

pub fn search(query: &str, limit: usize, json_out: bool, source: Option<&Path>) -> Result<()> {
    let prefix = source.map(|p| {
        sources::expand(&p.display().to_string())
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    });
    let hits = search_hits(query, limit, prefix.as_deref())?;
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
        println!(
            "── {:.3}  {}  [{}]",
            h.score, h.file_path, h.chunk_index
        );
        for line in h.chunk_text.lines().take(4) {
            println!("  {}", line);
        }
        println!();
    }
    Ok(())
}

/// Shared search path: prefer the daemon (it owns the NPU) and fall
/// back to in-process embedding if the daemon is down. Used by
/// `sy knowledge search`, `sy knowledge pick`, and the embedded MCP
/// server's `tool_search`. Without this, every consumer loads its
/// own ORT session and fights the daemon for /dev/accel/accel0 —
/// the loser silently downgrades to CUDA / CPU.
pub fn search_hits(
    query: &str,
    limit: usize,
    prefix: Option<&str>,
) -> Result<Vec<ipc::HitRow>> {
    if let Some(hits) = try_daemon_search(query, limit, prefix)? {
        return Ok(hits);
    }
    // Daemon down / unreachable → embed in-process and hit qdrant
    // directly. Keeps `sy knowledge search` working offline.
    let vec = embed::embed_one(query)?;
    let hits = qdrant::search(&vec, limit, prefix)?;
    Ok(hits
        .into_iter()
        .map(|h| ipc::HitRow {
            score: h.score,
            file_path: h.payload.file_path,
            chunk_index: h.payload.chunk_index,
            chunk_text: h.payload.chunk_text,
        })
        .collect())
}

fn try_daemon_search(
    query: &str,
    limit: usize,
    prefix: Option<&str>,
) -> Result<Option<Vec<ipc::HitRow>>> {
    // Liveness probe via the daemon's status snapshot — same shape
    // as `sync()` uses to decide whether to delegate FullResync.
    let alive = status::load()
        .ok()
        .map(|s| status::is_fresh(&s) && s.daemon_running)
        .unwrap_or(false);
    if !alive {
        return Ok(None);
    }
    let req = ipc::Req::Search {
        query: query.to_string(),
        limit,
        prefix: prefix.map(String::from),
    };
    match ipc::request(&req) {
        Ok(ipc::Resp::Search { hits }) => Ok(Some(hits)),
        Ok(ipc::Resp::Error { msg }) => {
            anyhow::bail!("daemon search: {msg}")
        }
        Ok(other) => anyhow::bail!("daemon: unexpected response {other:?}"),
        Err(ipc::IpcError::DaemonDown) => Ok(None),
        Err(ipc::IpcError::Wire(e)) => Err(e.context("ipc request")),
    }
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
    walker: WalkBuilder,
    glob_filter: Option<manifest::ManifestGlobFilter>,
    max_file_bytes: u64,
    tags: Vec<String>,
}

fn explicit_job(root: &Path) -> IndexJob {
    let mut wb = WalkBuilder::new(root);
    wb.hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true);
    let job_root = root.to_path_buf();
    wb.filter_entry(move |dent| {
        if !dent.file_type().map_or(false, |ft| ft.is_dir()) {
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
    // Per-file: (path, key, hash, chunks, source_tag, tags)
    let mut pending_files: Vec<(
        PathBuf,
        String,
        String,
        Vec<chunk::Chunk>,
        String,
        Vec<String>,
    )> = Vec::new();

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
                idx.files.get(&key).map(|e| e.mtime == mtime).unwrap_or(false)
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
                        eprintln!(
                            "sy knowledge: skip {} ({})",
                            p.display(),
                            reason.label()
                        );
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

            let chunks = chunk::chunk(&text);
            if chunks.is_empty() {
                report.skipped += 1;
                continue;
            }
            if !full_resync {
                if let Some(e) = idx.files.remove(&key) {
                    qdrant::delete_points(&e.point_ids)?;
                }
            }
            pending_files.push((
                p.to_path_buf(),
                key,
                hash,
                chunks,
                job.source_tag.clone(),
                job.tags.clone(),
            ));
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
        let chunks = &item.3;
        for (ci, c) in chunks.iter().enumerate() {
            batch_texts.push(c.text.clone());
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

    for (i, (path, key, hash, _chunks, _src, _tags)) in pending_files.into_iter().enumerate() {
        // Only commit files whose chunks all made it into qdrant. After a
        // mid-pass cancel, late-pending files have empty point_ids vecs —
        // skip them so the next pass treats them as still-changed.
        if file_point_ids[i].is_empty() {
            continue;
        }
        idx.files.insert(
            key,
            state::FileEntry {
                mtime: state::mtime_secs(&path),
                content_hash: hash,
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

fn flush_batch(
    texts: &mut Vec<String>,
    meta: &mut Vec<(usize, usize)>,
    pending: &[(
        PathBuf,
        String,
        String,
        Vec<chunk::Chunk>,
        String,
        Vec<String>,
    )],
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
        let (path, key, hash, chunks, src, tags) = &pending[fi];
        let chunk = &chunks[ci];
        let id = chunk::point_id(key, chunk.index);
        file_point_ids[fi].push(id.clone());
        points.push(Point {
            id,
            vector: vec,
            payload: PointPayload {
                source: src.clone(),
                file_path: key.clone(),
                chunk_index: chunk.index,
                chunk_text: chunk.text.clone(),
                file_mtime: state::mtime_secs(path),
                content_hash: hash.clone(),
                tags: tags.clone(),
            },
        });
        report.chunks += 1;
    }
    qdrant::upsert(&points)?;
    texts.clear();
    meta.clear();
    ctx.after_batch();
    Ok(())
}

/// Helper used by other modules — write to wl-copy for any "copy result"
/// flows we may add later. Not called from the main CLI today; kept for
/// parity with the stack module's affordances.
#[allow(dead_code)]
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .spawn()
        .context("wl-copy")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let _ = child.wait();
    Ok(())
}

const SY_KNOWLEDGE_UNIT: &str = include_str!(
    "../../configs/systemd/system/sy-knowledge.service"
);

const UNIT_DEST: &str = "/etc/systemd/system/sy-knowledge.service";

pub fn install_service(dry_run: bool) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .context("USER/LOGNAME not set")?;
    let uid = unsafe { libc::getuid() };

    let mut env = minijinja::Environment::new();
    env.add_template("unit", SY_KNOWLEDGE_UNIT)
        .context("parse unit template")?;
    let rendered = env
        .get_template("unit")
        .unwrap()
        .render(minijinja::context!(home, user, uid))
        .context("render unit template")?;

    let bin = PathBuf::from(&home).join(".local/bin/sy");
    if !bin.is_file() {
        anyhow::bail!(
            "sy binary not at {} — run `sy apply` first to install it",
            bin.display()
        );
    }

    if dry_run {
        println!("--- {UNIT_DEST} ---\n{rendered}");
        println!("--- selinux ---");
        println!("sudo semanage fcontext -a -t bin_t '{}'", bin.display());
        println!("sudo restorecon -v {}", bin.display());
        println!("--- systemd ---");
        println!("sudo install -m 0644 <rendered> {UNIT_DEST}");
        println!("sudo systemctl daemon-reload");
        println!("sudo systemctl enable --now sy-knowledge.service");
        return Ok(());
    }

    // 1. Drop the rendered unit at /etc/systemd/system/. We have to go
    //    through a tempfile + `sudo install` because the destination
    //    isn't writeable as the caller.
    let tmp = std::env::temp_dir().join(format!("sy-knowledge.service.{}", uid));
    std::fs::write(&tmp, rendered.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    sudo(
        &["install", "-m", "0644", &tmp.display().to_string(), UNIT_DEST],
        "install unit file",
    )?;
    let _ = std::fs::remove_file(&tmp);

    // 2. SELinux: relabel ~/.local/bin/sy as bin_t so system systemd can
    //    exec it (default label there is gconf_home_t, status=203/EXEC).
    //    Register the file-context rule so future `restorecon` keeps it.
    let bin_str = bin.display().to_string();
    let fcontext_pattern = format!("{}(/.*)?", bin_str);
    // semanage fcontext -a is idempotent only the first time; subsequent
    // calls fail with "rule already defined". Probe -l and skip if
    // already present.
    let existing = Command::new("sudo")
        .args(["semanage", "fcontext", "-l"])
        .output();
    let already = existing
        .as_ref()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().any(|l| l.contains(&bin_str))
        })
        .unwrap_or(false);
    if !already {
        sudo(
            &["semanage", "fcontext", "-a", "-t", "bin_t", &fcontext_pattern],
            "register selinux fcontext",
        )?;
    }
    sudo(&["restorecon", "-v", &bin_str], "restorecon binary")?;

    // 3. Reload systemd, enable + start the unit. If a transient unit of
    //    the same name is loaded (from a prior `systemd-run --unit=`),
    //    stop it first.
    let _ = Command::new("sudo")
        .args(["systemctl", "stop", "sy-knowledge.service"])
        .status();
    sudo(
        &["systemctl", "daemon-reload"],
        "systemctl daemon-reload",
    )?;
    sudo(
        &["systemctl", "enable", "--now", "sy-knowledge.service"],
        "systemctl enable --now",
    )?;

    println!("sy-knowledge.service installed and started.");
    println!("status: sudo systemctl status sy-knowledge.service");
    println!("logs:   journalctl -u sy-knowledge.service -f");
    Ok(())
}

fn sudo(args: &[&str], what: &str) -> Result<()> {
    let st = Command::new("sudo")
        .args(args)
        .status()
        .with_context(|| format!("spawn sudo for {what}"))?;
    if !st.success() {
        anyhow::bail!("sudo {what} failed (exit {:?})", st.code());
    }
    Ok(())
}
