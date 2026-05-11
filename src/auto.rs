//! `sy auto-configure` — system probe + opinionated defaults.
//!
//! Each module that wants to contribute defaults exposes a function
//! returning a `Vec<Detector>`. This module aggregates them, runs the
//! probes against a shared `ProbeEnv`, prints a plan, and (with
//! `--apply`) commits the changes.
//!
//! Two action shapes today, both knowledge-flavoured because knowledge
//! is the only module wired in. Adding `sy stack` defaults later is just
//! another `Action::*` variant + a detector list.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;

use crate::knowledge::sources::{self, SourceMode};

#[derive(Subcommand)]
pub enum AutoCmd {
    /// Probe the system and print/apply opinionated defaults.
    /// Default mode is dry-run; pass `--apply` to commit.
    Configure {
        /// Apply the plan (write to sy.toml + drop qdr.toml files).
        #[arg(long)]
        apply: bool,
        /// Emit the plan as JSON.
        #[arg(long)]
        json: bool,
        /// Restrict to detector ids (comma-separated).
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Skip detector ids (comma-separated).
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        /// Overwrite existing qdr.toml files when DropQdrManifest fires.
        #[arg(long)]
        force: bool,
    },
    /// List built-in detectors and whether each is on by default.
    ListDetectors {
        #[arg(long)]
        json: bool,
    },
}

pub fn dispatch(cmd: AutoCmd) -> Result<()> {
    match cmd {
        AutoCmd::Configure {
            apply,
            json,
            only,
            skip,
            force,
        } => configure(apply, json, &only, &skip, force),
        AutoCmd::ListDetectors { json } => list_detectors(json),
    }
}

// ─── Types ──────────────────────────────────────────────────────────────

pub struct Detector {
    pub id: &'static str,
    pub label: &'static str,
    pub default_on: bool,
    pub probe: fn(&ProbeEnv) -> Vec<Suggestion>,
}

#[derive(Debug, Clone)]
pub struct Suggestion {
    pub detector_id: &'static str,
    pub action: Action,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    /// Append a `[[knowledge.sources]]` entry to `sy.toml`.
    AddKnowledgeSource { path: PathBuf, mode: SourceMode },
    /// Write a `qdr.toml` at the given folder root.
    DropQdrManifest {
        folder: PathBuf,
        template: ManifestTemplate,
    },
    /// Register `sy-knowledge` in an agent's MCP-server config. The
    /// apply step is idempotent — if the entry already matches the
    /// proposed `command`/`args`, it's reported as a no-op.
    AddMcpServer {
        agent: crate::auto_mcp::McpAgent,
        name: String,
        command: String,
        args: Vec<String>,
    },
    /// Drop `sy-knowledge` from an agent's MCP-server config. No-op if
    /// the entry isn't present.
    RemoveMcpServer {
        agent: crate::auto_mcp::McpAgent,
        name: String,
    },
    /// Non-actionable advice (e.g. "to make Telegram messages searchable,
    /// run …"). Printed under "Hints:" in the plan; ignored on --apply.
    Hint { message: String },
}

#[derive(Debug, Clone, Default)]
pub struct ManifestTemplate {
    pub name: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub tags: Vec<String>,
    pub max_depth: Option<usize>,
    pub max_file_bytes: Option<u64>,
}

pub struct ProbeEnv {
    pub home: PathBuf,
    pub xdg_documents: Option<PathBuf>,
    pub xdg_download: Option<PathBuf>,
    pub installed_bins: HashSet<String>,
    pub flatpak_apps: HashSet<String>,
}

impl ProbeEnv {
    pub fn build() -> Self {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"));
        let xdg_documents = resolve_xdg_dir(&home, "DOCUMENTS", "Documents");
        let xdg_download = resolve_xdg_dir(&home, "DOWNLOAD", "Downloads");
        let installed_bins = probe_bins(&[
            "obsidian", "logseq", "joplin", "zettlr", "anki", "zotero", "calibre",
            "code", "cursor", "lmstudio", "bruno", "telegram-desktop",
        ]);
        let flatpak_apps = probe_flatpak_apps();
        Self {
            home,
            xdg_documents,
            xdg_download,
            installed_bins,
            flatpak_apps,
        }
    }

    /// Convenience: union of binary names and flatpak app ids.
    pub fn has_app(&self, key: &str) -> bool {
        self.installed_bins.contains(key)
            || self.flatpak_apps.iter().any(|a| a.contains(key))
    }
}

fn resolve_xdg_dir(home: &Path, key: &str, fallback_basename: &str) -> Option<PathBuf> {
    if let Ok(out) = std::process::Command::new("xdg-user-dir").arg(key).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let p = PathBuf::from(s);
            if p.is_dir() && p != home {
                return Some(p);
            }
        }
    }
    let cfg = home.join(".config").join("user-dirs.dirs");
    if let Ok(s) = std::fs::read_to_string(&cfg) {
        let needle = format!("XDG_{key}_DIR=");
        for line in s.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix(&needle) {
                let v = rest.trim_matches('"');
                let expanded = v.replace("$HOME", home.to_str().unwrap_or(""));
                let p = PathBuf::from(expanded);
                if p.is_dir() {
                    return Some(p);
                }
            }
        }
    }
    let p = home.join(fallback_basename);
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

fn probe_bins(names: &[&str]) -> HashSet<String> {
    let mut out = HashSet::new();
    for n in names {
        if crate::which(n) {
            out.insert(n.to_string());
        }
    }
    out
}

fn probe_flatpak_apps() -> HashSet<String> {
    if !crate::which("flatpak") {
        return HashSet::new();
    }
    std::process::Command::new("flatpak")
        .args(["list", "--app", "--columns=application"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// ─── Detector aggregation ───────────────────────────────────────────────

pub fn detectors() -> Vec<Detector> {
    crate::knowledge::autoconfig::detectors()
}

fn select_detectors<'a>(
    all: &'a [Detector],
    only: &[String],
    skip: &[String],
) -> Vec<&'a Detector> {
    let only_set: HashSet<&str> = only.iter().map(|s| s.as_str()).collect();
    let skip_set: HashSet<&str> = skip.iter().map(|s| s.as_str()).collect();
    all.iter()
        .filter(|d| {
            if !only_set.is_empty() {
                only_set.contains(d.id)
            } else if d.default_on {
                !skip_set.contains(d.id)
            } else {
                only_set.contains(d.id) // off-by-default unless explicitly --only
            }
        })
        .collect()
}

// ─── Configure ──────────────────────────────────────────────────────────

pub fn configure(apply: bool, json_out: bool, only: &[String], skip: &[String], force: bool) -> Result<()> {
    let env = ProbeEnv::build();
    let all = detectors();
    let chosen = select_detectors(&all, only, skip);

    let mut suggestions: Vec<Suggestion> = Vec::new();
    for d in &chosen {
        let mut s = (d.probe)(&env);
        suggestions.append(&mut s);
    }

    if json_out {
        emit_json_plan(&chosen, &suggestions, apply, force);
    } else {
        emit_text_plan(&chosen, &suggestions, apply);
    }

    if !apply {
        return Ok(());
    }
    apply_suggestions(&suggestions, force, json_out)
}

fn emit_text_plan(chosen: &[&Detector], suggestions: &[Suggestion], apply: bool) {
    let actionable = suggestions
        .iter()
        .filter(|s| !matches!(s.action, Action::Hint { .. }))
        .count();
    let hints = suggestions.len() - actionable;
    eprintln!(
        "sy auto-configure: {} detector{} fired, {} suggestion{} ({} hint{}){}",
        chosen.len(),
        if chosen.len() == 1 { "" } else { "s" },
        actionable,
        if actionable == 1 { "" } else { "s" },
        hints,
        if hints == 1 { "" } else { "s" },
        if apply { " [applying]" } else { " [dry-run]" }
    );
    if suggestions.is_empty() {
        println!("(nothing to do — your environment looks already configured)");
        return;
    }

    let mut by_detector: BTreeMap<&str, Vec<&Suggestion>> = BTreeMap::new();
    for s in suggestions {
        by_detector.entry(s.detector_id).or_default().push(s);
    }
    for (id, items) in &by_detector {
        let label = chosen
            .iter()
            .find(|d| d.id == *id)
            .map(|d| d.label)
            .unwrap_or(id);
        println!();
        println!("── {id}  ({label})");
        for s in items {
            match &s.action {
                Action::AddKnowledgeSource { path, mode } => {
                    let mode_s = match mode {
                        SourceMode::Explicit => "explicit",
                        SourceMode::Discover => "discover",
                    };
                    println!(
                        "  + sy.toml [[knowledge.sources]]  {}  (mode={mode_s})",
                        path.display()
                    );
                    println!("    {}", s.reason);
                }
                Action::DropQdrManifest { folder, template } => {
                    println!("  + qdr.toml at {}", folder.display());
                    if !template.include.is_empty() {
                        println!("    include: {:?}", template.include);
                    }
                    if !template.exclude.is_empty() {
                        println!("    exclude: {:?}", template.exclude);
                    }
                    if !template.tags.is_empty() {
                        println!("    tags:    {:?}", template.tags);
                    }
                    if let Some(b) = template.max_file_bytes {
                        println!("    max_file_bytes: {b}");
                    }
                    println!("    {}", s.reason);
                }
                Action::AddMcpServer {
                    agent,
                    name,
                    command,
                    args,
                } => {
                    let st = crate::auto_mcp::read_state(*agent);
                    let already = match &st.as_ref().and_then(|s| s.registered.as_ref()) {
                        Some(e) => e.command == *command && e.args == *args,
                        None => false,
                    };
                    let verb = if already { "=" } else { "+" };
                    println!(
                        "  {verb} mcp [{}] {} → {} {}",
                        agent.id(),
                        name,
                        command,
                        args.join(" ")
                    );
                    if !already {
                        println!("    {}", s.reason);
                    }
                }
                Action::RemoveMcpServer { agent, name } => {
                    println!("  - mcp [{}] {}", agent.id(), name);
                    println!("    {}", s.reason);
                }
                Action::Hint { message } => {
                    println!("  💡 {message}");
                }
            }
        }
    }
    if !apply {
        println!();
        println!("re-run with --apply to commit these changes.");
    }
}

fn emit_json_plan(chosen: &[&Detector], suggestions: &[Suggestion], apply: bool, force: bool) {
    let arr: Vec<_> = suggestions.iter().map(suggestion_to_json).collect();
    let detectors_json: Vec<_> = chosen
        .iter()
        .map(|d| json!({"id": d.id, "label": d.label, "default_on": d.default_on}))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "apply": apply,
            "force": force,
            "detectors": detectors_json,
            "suggestions": arr,
        }))
        .unwrap_or_default()
    );
}

fn suggestion_to_json(s: &Suggestion) -> serde_json::Value {
    match &s.action {
        Action::AddKnowledgeSource { path, mode } => json!({
            "detector": s.detector_id,
            "kind": "add-knowledge-source",
            "path": path.display().to_string(),
            "mode": match mode { SourceMode::Explicit => "explicit", SourceMode::Discover => "discover" },
            "reason": s.reason,
        }),
        Action::DropQdrManifest { folder, template } => json!({
            "detector": s.detector_id,
            "kind": "drop-qdr-manifest",
            "folder": folder.display().to_string(),
            "manifest": {
                "name": template.name,
                "include": template.include,
                "exclude": template.exclude,
                "tags": template.tags,
                "max_depth": template.max_depth,
                "max_file_bytes": template.max_file_bytes,
            },
            "reason": s.reason,
        }),
        Action::AddMcpServer { agent, name, command, args } => json!({
            "detector": s.detector_id,
            "kind": "add-mcp-server",
            "agent": agent.id(),
            "name": name,
            "command": command,
            "args": args,
            "reason": s.reason,
        }),
        Action::RemoveMcpServer { agent, name } => json!({
            "detector": s.detector_id,
            "kind": "remove-mcp-server",
            "agent": agent.id(),
            "name": name,
            "reason": s.reason,
        }),
        Action::Hint { message } => json!({
            "detector": s.detector_id,
            "kind": "hint",
            "message": message,
            "reason": s.reason,
        }),
    }
}

// ─── Apply ──────────────────────────────────────────────────────────────

fn apply_suggestions(suggestions: &[Suggestion], force: bool, quiet: bool) -> Result<()> {
    let mut added = 0usize;
    let mut wrote = 0usize;
    let mut mcp_changed = 0usize;
    let mut skipped = 0usize;
    let mut errs: Vec<String> = Vec::new();
    let mut sources_dirty = false;
    let mut interactive_agents: std::collections::HashSet<&'static str> =
        std::collections::HashSet::new();

    for s in suggestions {
        match &s.action {
            Action::Hint { .. } => {}
            Action::AddKnowledgeSource { path, mode } => {
                match sources::add(path, false, *mode) {
                    Ok(true) => {
                        added += 1;
                        sources_dirty = true;
                        if !quiet {
                            eprintln!("  + source {}", path.display());
                        }
                    }
                    Ok(false) => {
                        skipped += 1;
                        if !quiet {
                            eprintln!("  = source {} (already registered)", path.display());
                        }
                    }
                    Err(e) => errs.push(format!("add source {}: {e}", path.display())),
                }
            }
            Action::DropQdrManifest { folder, template } => {
                match write_manifest(folder, template, force) {
                    Ok(true) => {
                        wrote += 1;
                        if !quiet {
                            eprintln!("  + qdr.toml {}", folder.display());
                        }
                    }
                    Ok(false) => {
                        skipped += 1;
                        if !quiet {
                            eprintln!("  = qdr.toml {} (exists; --force to overwrite)", folder.display());
                        }
                    }
                    Err(e) => errs.push(format!("write manifest {}: {e}", folder.display())),
                }
            }
            Action::AddMcpServer {
                agent,
                name,
                command,
                args,
            } => {
                let entry = crate::auto_mcp::McpEntry {
                    command: command.clone(),
                    args: args.clone(),
                };
                match crate::auto_mcp::apply_add(*agent, &entry) {
                    Ok(true) => {
                        mcp_changed += 1;
                        if matches!(
                            agent,
                            crate::auto_mcp::McpAgent::Claude | crate::auto_mcp::McpAgent::Cursor
                        ) {
                            interactive_agents.insert(agent.label());
                        }
                        if !quiet {
                            eprintln!("  + mcp [{}] {}", agent.id(), name);
                        }
                    }
                    Ok(false) => {
                        skipped += 1;
                        if !quiet {
                            eprintln!("  = mcp [{}] {} (already wired)", agent.id(), name);
                        }
                    }
                    Err(e) => errs.push(format!("mcp add {}: {e}", agent.id())),
                }
            }
            Action::RemoveMcpServer { agent, name } => {
                match crate::auto_mcp::apply_remove(*agent) {
                    Ok(true) => {
                        mcp_changed += 1;
                        if !quiet {
                            eprintln!("  - mcp [{}] {}", agent.id(), name);
                        }
                    }
                    Ok(false) => {
                        skipped += 1;
                        if !quiet {
                            eprintln!("  = mcp [{}] {} (not present)", agent.id(), name);
                        }
                    }
                    Err(e) => errs.push(format!("mcp remove {}: {e}", agent.id())),
                }
            }
        }
    }

    if !interactive_agents.is_empty() && !quiet {
        let names: Vec<&&str> = interactive_agents.iter().collect();
        eprintln!(
            "sy auto-configure: restart {} to pick up the new MCP server",
            names
                .into_iter()
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let _ = mcp_changed;

    if sources_dirty {
        sources::notify_daemon_refresh();
    }

    if !quiet {
        eprintln!(
            "sy auto-configure: applied. sources +{added}, qdr.toml +{wrote}, skipped {skipped}, errors {}",
            errs.len()
        );
    }
    if !errs.is_empty() {
        for e in &errs {
            eprintln!("  ! {e}");
        }
        anyhow::bail!("{} action(s) failed", errs.len());
    }
    Ok(())
}

fn write_manifest(folder: &Path, t: &ManifestTemplate, force: bool) -> Result<bool> {
    let path = folder.join("qdr.toml");
    if path.exists() && !force {
        return Ok(false);
    }
    let body = render_manifest(t);
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

fn render_manifest(t: &ManifestTemplate) -> String {
    let mut s = String::new();
    s.push_str("# Auto-generated by `sy auto-configure`. Edit freely.\n");
    s.push_str("[knowledge]\n");
    s.push_str(&format!("name    = {}\n", toml_str(&t.name)));
    if !t.tags.is_empty() {
        s.push_str(&format!("tags    = {}\n", toml_str_array(&t.tags)));
    }
    if !t.include.is_empty() {
        s.push_str(&format!("include = {}\n", toml_str_array(&t.include)));
    }
    if !t.exclude.is_empty() {
        s.push_str(&format!("exclude = {}\n", toml_str_array(&t.exclude)));
    }
    if let Some(d) = t.max_depth {
        s.push_str(&format!("max_depth = {d}\n"));
    }
    if let Some(b) = t.max_file_bytes {
        s.push_str(&format!("max_file_bytes = {b}\n"));
    }
    s
}

fn toml_str(s: &str) -> String {
    // Minimal TOML basic-string escaping: backslash + quote.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_str_array(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| toml_str(s)).collect();
    format!("[{}]", inner.join(", "))
}

// ─── List detectors ─────────────────────────────────────────────────────

fn list_detectors(json_out: bool) -> Result<()> {
    let all = detectors();
    if json_out {
        let arr: Vec<_> = all
            .iter()
            .map(|d| json!({"id": d.id, "label": d.label, "default_on": d.default_on}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    println!("{:<22} {:<5} {}", "ID", "ON", "LABEL");
    for d in &all {
        let on = if d.default_on { "yes" } else { "no" };
        println!("{:<22} {:<5} {}", d.id, on, d.label);
    }
    Ok(())
}
