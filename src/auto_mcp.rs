//! Per-agent MCP-config writers + status readers used by
//! `sy auto-configure`'s `mcp-claude` / `mcp-cursor` / `mcp-codex` /
//! `mcp-gemini` / `mcp-antigravity` / `mcp-agents` detectors.
//!
//! Each agent stores its registered MCP servers in a slightly different
//! place and format. We keep the writers symmetric: `read_state` returns
//! whether the named entry is registered + the `(command, args)` pair
//! we'd compare against; `apply_add` / `apply_remove` perform an atomic
//! edit and return `true` only when the file actually changed.

use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

/// Canonical name used in every agent's `mcpServers` map.
pub const SERVER_NAME: &str = "sy-knowledge";

/// One of the agents we know how to write into. Drives the apply step's
/// match; new agents drop in here + a writer pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAgent {
    Claude,
    Cursor,
    Gemini,
    Codex,
    Goose,
    Antigravity,
    Agents,
}

impl McpAgent {
    pub fn id(self) -> &'static str {
        match self {
            McpAgent::Claude => "claude",
            McpAgent::Cursor => "cursor",
            McpAgent::Gemini => "gemini",
            McpAgent::Codex => "codex",
            McpAgent::Goose => "goose",
            McpAgent::Antigravity => "antigravity",
            McpAgent::Agents => "agents",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            McpAgent::Claude => "Claude Code",
            McpAgent::Cursor => "Cursor IDE",
            McpAgent::Gemini => "Gemini CLI",
            McpAgent::Codex => "OpenAI Codex CLI",
            McpAgent::Goose => "Goose",
            McpAgent::Antigravity => "Google Antigravity",
            McpAgent::Agents => "Custom agents (~/.agents/)",
        }
    }

    /// Is this agent's MCP config writable yet? Antigravity / `~/.agents/`
    /// have no canonical path so we ship hint-only detectors for them.
    pub fn is_writable(self) -> bool {
        !matches!(self, McpAgent::Antigravity | McpAgent::Agents)
    }
}

#[derive(Debug, Clone)]
pub struct McpEntry {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CurrentState {
    /// Path to the agent's MCP-config file (whether or not it exists).
    pub path: PathBuf,
    /// `true` if the config file is present and *we know how to write
    /// to it*. Antigravity / `~/.agents/` always return `false` here.
    pub writable: bool,
    /// Currently registered entry under `mcpServers["sy-knowledge"]`,
    /// when present.
    pub registered: Option<McpEntry>,
}

/// Probe the agent's config file. Returns `None` when the agent's home
/// dir doesn't exist (so we don't propose creating a config for an
/// agent the user hasn't installed).
pub fn read_state(agent: McpAgent) -> Option<CurrentState> {
    let home = home_dir()?;
    match agent {
        McpAgent::Claude => {
            let path = home.join(".claude.json");
            if !home.join(".claude").exists() && !path.exists() {
                return None;
            }
            Some(CurrentState {
                registered: read_json_entry(&path, SERVER_NAME).flatten(),
                writable: true,
                path,
            })
        }
        McpAgent::Cursor => {
            let cursor_home = home.join(".cursor");
            if !cursor_home.is_dir() {
                return None;
            }
            let path = cursor_home.join("mcp.json");
            Some(CurrentState {
                registered: read_json_entry(&path, SERVER_NAME).flatten(),
                writable: true,
                path,
            })
        }
        McpAgent::Gemini => {
            let gemini_home = home.join(".gemini");
            if !gemini_home.is_dir() {
                return None;
            }
            let path = gemini_home.join("settings.json");
            Some(CurrentState {
                registered: read_json_entry(&path, SERVER_NAME).flatten(),
                writable: true,
                path,
            })
        }
        McpAgent::Codex => {
            let codex_home = home.join(".codex");
            if !codex_home.is_dir() {
                return None;
            }
            let path = codex_home.join("config.toml");
            Some(CurrentState {
                registered: read_codex_entry(&path, SERVER_NAME),
                writable: true,
                path,
            })
        }
        McpAgent::Goose => {
            let goose_home = home.join(".config/goose");
            if !goose_home.is_dir() {
                return None;
            }
            let path = goose_home.join("config.yaml");
            Some(CurrentState {
                registered: read_goose_entry(&path, SERVER_NAME),
                writable: true,
                path,
            })
        }
        McpAgent::Antigravity => {
            let p = home.join(".antigravity");
            if !p.is_dir() {
                return None;
            }
            Some(CurrentState {
                path: p,
                writable: false,
                registered: None,
            })
        }
        McpAgent::Agents => {
            let p = home.join(".agents");
            if !p.is_dir() {
                return None;
            }
            Some(CurrentState {
                path: p,
                writable: false,
                registered: None,
            })
        }
    }
}

/// Write `mcpServers["sy-knowledge"] = {command, args}` into the agent's
/// config. Returns `Ok(true)` when the file changed (entry was missing
/// or differed); `Ok(false)` when the existing entry already matched.
pub fn apply_add(agent: McpAgent, entry: &McpEntry) -> Result<bool> {
    let Some(state) = read_state(agent) else {
        anyhow::bail!("{} home dir missing; nothing to write", agent.label());
    };
    if !state.writable {
        anyhow::bail!(
            "{}: no canonical MCP config path is supported yet",
            agent.label()
        );
    }
    if let Some(existing) = &state.registered {
        if existing.command == entry.command && existing.args == entry.args {
            return Ok(false);
        }
    }
    match agent {
        McpAgent::Claude | McpAgent::Cursor | McpAgent::Gemini => {
            write_json_entry(&state.path, SERVER_NAME, entry)?;
        }
        McpAgent::Codex => write_codex_entry(&state.path, SERVER_NAME, entry)?,
        McpAgent::Goose => write_goose_entry(&state.path, SERVER_NAME, entry)?,
        McpAgent::Antigravity | McpAgent::Agents => unreachable!(),
    }
    Ok(true)
}

/// Remove `mcpServers["sy-knowledge"]` from the agent's config.
/// Returns `Ok(true)` when the file changed; `Ok(false)` when the entry
/// wasn't present (no-op).
pub fn apply_remove(agent: McpAgent) -> Result<bool> {
    let Some(state) = read_state(agent) else {
        return Ok(false);
    };
    if !state.writable || state.registered.is_none() {
        return Ok(false);
    }
    match agent {
        McpAgent::Claude | McpAgent::Cursor | McpAgent::Gemini => {
            remove_json_entry(&state.path, SERVER_NAME)
        }
        McpAgent::Codex => remove_codex_entry(&state.path, SERVER_NAME),
        McpAgent::Goose => remove_goose_entry(&state.path, SERVER_NAME),
        McpAgent::Antigravity | McpAgent::Agents => Ok(false),
    }
}

// ── JSON (.claude.json, .cursor/mcp.json, .gemini/settings.json) ────────

fn read_json_entry(path: &Path, name: &str) -> Option<Option<McpEntry>> {
    if !path.is_file() {
        return Some(None);
    }
    let body = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let entry = v
        .get("mcpServers")
        .and_then(|o| o.get(name))
        .and_then(json_to_entry);
    Some(entry)
}

fn write_json_entry(path: &Path, name: &str, entry: &McpEntry) -> Result<()> {
    let mut root = load_json_root(path)?;
    let map = ensure_object(&mut root);
    let servers = map
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: `mcpServers` is not a JSON object", path.display()))?;
    servers.insert(
        name.to_string(),
        json!({ "command": entry.command, "args": entry.args }),
    );
    atomic_write_json(path, &root)
}

fn remove_json_entry(path: &Path, name: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut root = load_json_root(path)?;
    let Some(servers) = root.get_mut("mcpServers").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    if servers.remove(name).is_none() {
        return Ok(false);
    }
    atomic_write_json(path, &root)?;
    Ok(true)
}

fn load_json_root(path: &Path) -> Result<Value> {
    if !path.is_file() {
        return Ok(Value::Object(Map::new()));
    }
    let body = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if body.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

fn ensure_object(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    v.as_object_mut().expect("just inserted")
}

fn atomic_write_json(path: &Path, v: &Value) -> Result<()> {
    let body = serde_json::to_vec_pretty(v)?;
    let tmp = path.with_extension(format!(
        "{}.sy-tmp",
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("json")
    ));
    if let Some(parent) = tmp.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).ok();
        }
    }
    {
        let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&body)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

fn json_to_entry(v: &Value) -> Option<McpEntry> {
    let command = v.get("command")?.as_str()?.to_string();
    let args = v
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(McpEntry { command, args })
}

// ── TOML (.codex/config.toml) ──────────────────────────────────────────
//
// Codex CLI registers MCP servers under `[mcp_servers.<name>]` with the
// shape `{command = "...", args = [...]}`. We use `toml_edit::DocumentMut`
// so user-authored comments and key order survive round-trips.

fn read_codex_entry(path: &Path, name: &str) -> Option<McpEntry> {
    if !path.is_file() {
        return None;
    }
    let body = fs::read_to_string(path).ok()?;
    let doc: toml_edit::DocumentMut = body.parse().ok()?;
    let table = doc.get("mcp_servers")?.as_table()?;
    let entry = table.get(name)?.as_table()?;
    let command = entry.get("command")?.as_str()?.to_string();
    let args = entry
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(McpEntry { command, args })
}

fn write_codex_entry(path: &Path, name: &str, entry: &McpEntry) -> Result<()> {
    use toml_edit::{value, Array, Item, Table};
    let body = if path.is_file() {
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut =
        body.parse().with_context(|| format!("parse {}", path.display()))?;
    if doc.get("mcp_servers").is_none() {
        doc.insert("mcp_servers", Item::Table(Table::new()));
    }
    let servers = doc["mcp_servers"]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: `mcp_servers` is not a table", path.display()))?;
    // Recreate the entry table cleanly so stale keys don't linger.
    let mut entry_t = Table::new();
    entry_t.insert("command", value(entry.command.clone()));
    let mut args_arr = Array::new();
    for a in &entry.args {
        args_arr.push(a.clone());
    }
    entry_t.insert("args", Item::Value(args_arr.into()));
    servers.insert(name, Item::Table(entry_t));
    atomic_write_str(path, &doc.to_string())
}

fn remove_codex_entry(path: &Path, name: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let body = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut =
        body.parse().with_context(|| format!("parse {}", path.display()))?;
    let Some(servers) = doc.get_mut("mcp_servers").and_then(|i| i.as_table_mut()) else {
        return Ok(false);
    };
    if servers.remove(name).is_none() {
        return Ok(false);
    }
    if servers.is_empty() {
        doc.as_table_mut().remove("mcp_servers");
    }
    atomic_write_str(path, &doc.to_string())?;
    Ok(true)
}

fn atomic_write_str(path: &Path, body: &str) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}.sy-tmp",
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("toml")
    ));
    {
        let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

// ── YAML (~/.config/goose/config.yaml) ─────────────────────────────────
//
// Goose registers MCP servers under `extensions:`, with the shape:
//   extensions:
//     sy-knowledge:
//       enabled: true
//       type: stdio
//       name: sy-knowledge
//       cmd: <command>
//       args: [knowledge, mcp]
//       timeout: 300
//       description: sy knowledge plane (semantic search)
// We use `serde_yml` (the maintained replacement for the deprecated
// `serde_yaml`) to load the YAML into a `Value`, mutate the entry, and
// dump it back. This loses YAML-specific niceties like anchors and
// comments — Goose's own machine-managed config doesn't use them, so
// this is acceptable in practice.

fn read_goose_entry(path: &Path, name: &str) -> Option<McpEntry> {
    if !path.is_file() {
        return None;
    }
    let body = fs::read_to_string(path).ok()?;
    let v: serde_yml::Value = serde_yml::from_str(&body).ok()?;
    let entry = v.get("extensions")?.get(name)?;
    let cmd = entry.get("cmd").and_then(|x| x.as_str())?.to_string();
    let args = entry
        .get("args")
        .and_then(|x| x.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(McpEntry { command: cmd, args })
}

fn write_goose_entry(path: &Path, name: &str, entry: &McpEntry) -> Result<()> {
    use serde_yml::{Mapping, Value};
    let mut root: Value = if path.is_file() {
        let body = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if body.trim().is_empty() {
            Value::Mapping(Mapping::new())
        } else {
            serde_yml::from_str(&body)
                .with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        Value::Mapping(Mapping::new())
    };
    let root_map = root
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: top-level is not a YAML mapping", path.display()))?;
    let extensions = root_map
        .entry(Value::String("extensions".into()))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    let extensions = extensions
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: `extensions` is not a YAML mapping", path.display()))?;

    // Build the entry. Goose calls the binary via `cmd` and reads tool
    // metadata from `description` / `name`; everything else mirrors what
    // the GUI's "add extension" flow writes.
    let mut e = Mapping::new();
    e.insert(Value::String("enabled".into()), Value::Bool(true));
    e.insert(Value::String("type".into()), Value::String("stdio".into()));
    e.insert(Value::String("name".into()), Value::String(name.into()));
    e.insert(
        Value::String("display_name".into()),
        Value::String("sy knowledge".into()),
    );
    e.insert(
        Value::String("description".into()),
        Value::String("Semantic search over sy's knowledge plane.".into()),
    );
    e.insert(
        Value::String("cmd".into()),
        Value::String(entry.command.clone()),
    );
    let args_seq = entry
        .args
        .iter()
        .map(|a| Value::String(a.clone()))
        .collect();
    e.insert(Value::String("args".into()), Value::Sequence(args_seq));
    e.insert(
        Value::String("env_keys".into()),
        Value::Sequence(Vec::new()),
    );
    e.insert(
        Value::String("envs".into()),
        Value::Mapping(Mapping::new()),
    );
    e.insert(Value::String("timeout".into()), Value::Number(300.into()));
    e.insert(Value::String("bundled".into()), Value::Bool(false));

    extensions.insert(Value::String(name.into()), Value::Mapping(e));

    atomic_write_str(path, &serde_yml::to_string(&root)?)
}

fn remove_goose_entry(path: &Path, name: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let body = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut root: serde_yml::Value =
        serde_yml::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    let Some(extensions) = root
        .get_mut("extensions")
        .and_then(|v| v.as_mapping_mut())
    else {
        return Ok(false);
    };
    let key = serde_yml::Value::String(name.into());
    if extensions.remove(&key).is_none() {
        return Ok(false);
    }
    atomic_write_str(path, &serde_yml::to_string(&root)?)?;
    Ok(true)
}

// ── helpers ────────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// Resolve the absolute path of the running `sy` binary so the entry
/// agents write into their MCP configs is robust to PATH mutations.
pub fn resolved_sy_command() -> String {
    if let Ok(p) = std::env::current_exe() {
        // Resolve symlinks so `~/.local/bin/sy` (often a symlink) ends
        // up as the real path, but only if canonicalize succeeds; on
        // failure fall back to the symlink itself.
        let resolved = std::fs::canonicalize(&p).unwrap_or(p);
        return resolved.display().to_string();
    }
    if let Ok(p) = which::which("sy") {
        return p.display().to_string();
    }
    "sy".to_string()
}

/// The command/args every detector emits. Stable across agents.
pub fn desired_entry() -> McpEntry {
    McpEntry {
        command: resolved_sy_command(),
        args: vec!["knowledge".into(), "mcp".into()],
    }
}

/// All agents we know about (writable + hint-only). Detectors and the
/// `sy knowledge mcp-status` command iterate this list.
pub const ALL_AGENTS: &[McpAgent] = &[
    McpAgent::Claude,
    McpAgent::Cursor,
    McpAgent::Codex,
    McpAgent::Gemini,
    McpAgent::Goose,
    McpAgent::Antigravity,
    McpAgent::Agents,
];
