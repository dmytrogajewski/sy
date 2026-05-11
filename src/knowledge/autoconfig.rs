//! Detectors that the top-level `sy auto-configure` aggregates. Each
//! detector probes a narrow slice of the system and returns suggestions
//! (knowledge sources to add, qdr.toml manifests to drop, or hints).
//!
//! Design notes:
//! - Probes are bounded. We never walk the full HOME — note-app probes
//!   stick to a small set of likely roots (XDG dirs + named subfolders).
//! - All file walks use `ignore::WalkBuilder` with gitignore-aware
//!   semantics to keep `target/`, `node_modules/`, `.cache/` etc. out.
//! - Suggestions never overwrite existing `qdr.toml` files (the apply
//!   step refuses unless `--force`).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use ignore::WalkBuilder;

use crate::auto::{Action, Detector, ManifestTemplate, ProbeEnv, Suggestion};
use crate::knowledge::sources::SourceMode;

/// All built-in detectors. Order matters only for plan readability.
pub fn detectors() -> Vec<Detector> {
    vec![
        Detector {
            id: "xdg-documents",
            label: "XDG Documents folder as discovery root",
            default_on: true,
            probe: probe_xdg_documents,
        },
        Detector {
            id: "xdg-downloads",
            label: "XDG Downloads folder as discovery root",
            default_on: true,
            probe: probe_xdg_downloads,
        },
        Detector {
            id: "home-knowledge-dir",
            label: "Curated ~/knowledge or ~/Knowledge folder",
            default_on: true,
            probe: probe_home_knowledge,
        },
        Detector {
            id: "telegram-exports",
            label: "Telegram chat exports (ChatExport_*, result.json)",
            default_on: true,
            probe: probe_telegram_exports,
        },
        Detector {
            id: "obsidian-vaults",
            label: "Obsidian vaults (.obsidian markers)",
            default_on: true,
            probe: probe_obsidian,
        },
        Detector {
            id: "logseq-graphs",
            label: "Logseq graphs (logseq/ + pages/ siblings)",
            default_on: true,
            probe: probe_logseq,
        },
        Detector {
            id: "joplin-export",
            label: "Joplin notes export folders",
            default_on: true,
            probe: probe_joplin,
        },
        Detector {
            id: "zettlr-projects",
            label: "Zettlr projects (.zettlr-roots)",
            default_on: true,
            probe: probe_zettlr,
        },
        Detector {
            id: "bruno-collections",
            label: "Bruno API collections (*.bru trees)",
            default_on: true,
            probe: probe_bruno,
        },
        Detector {
            id: "agent-histories",
            label: "AI-agent histories (.claude / .codex / .gemini / .cursor)",
            default_on: true,
            probe: probe_agent_histories,
        },
        Detector {
            id: "dotfiles",
            label: "Dotfiles repo (~/dotfiles or ~/.dotfiles)",
            default_on: false,
            probe: probe_dotfiles,
        },
        Detector {
            id: "mcp-claude",
            label: "Wire `sy-knowledge` MCP into Claude Code (~/.claude.json)",
            default_on: true,
            probe: probe_mcp_claude,
        },
        Detector {
            id: "mcp-cursor",
            label: "Wire `sy-knowledge` MCP into Cursor IDE (~/.cursor/mcp.json)",
            default_on: true,
            probe: probe_mcp_cursor,
        },
        Detector {
            id: "mcp-codex",
            label: "Wire `sy-knowledge` MCP into OpenAI Codex CLI (~/.codex/config.toml)",
            default_on: true,
            probe: probe_mcp_codex,
        },
        Detector {
            id: "mcp-gemini",
            label: "Wire `sy-knowledge` MCP into Gemini CLI (~/.gemini/settings.json)",
            default_on: true,
            probe: probe_mcp_gemini,
        },
        Detector {
            id: "mcp-goose",
            label: "Wire `sy-knowledge` MCP into Goose (~/.config/goose/config.yaml)",
            default_on: true,
            probe: probe_mcp_goose,
        },
        Detector {
            id: "mcp-antigravity",
            label: "Wire `sy-knowledge` MCP into Google Antigravity",
            default_on: false,
            probe: probe_mcp_antigravity,
        },
        Detector {
            id: "mcp-agents",
            label: "Wire `sy-knowledge` MCP into custom ~/.agents/ runtime",
            default_on: false,
            probe: probe_mcp_agents,
        },
    ]
}

// ─── Helpers ────────────────────────────────────────────────────────────

/// A small, named root list to scan for note-style content. Crucially does
/// NOT include $HOME root or ~/sources — those are too large to walk.
fn note_probe_roots(env: &ProbeEnv) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_dir() && !roots.contains(&p) {
            roots.push(p);
        }
    };
    if let Some(d) = &env.xdg_documents {
        push(d.clone());
    }
    for name in &["Documents", "Notes", "notes", "Obsidian", "Vault", "Vaults", "Knowledge", "knowledge"] {
        push(env.home.join(name));
    }
    roots
}

fn walk_capped(root: &Path, max_depth: usize) -> ignore::Walk {
    WalkBuilder::new(root)
        .max_depth(Some(max_depth))
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .build()
}

fn folder_basename(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("folder")
        .to_string()
}

fn drop_qdr_manifest(
    detector_id: &'static str,
    folder: PathBuf,
    template: ManifestTemplate,
    reason: String,
) -> Suggestion {
    Suggestion {
        detector_id,
        action: Action::DropQdrManifest { folder, template },
        reason,
    }
}

// ─── Detectors ──────────────────────────────────────────────────────────

fn probe_xdg_documents(env: &ProbeEnv) -> Vec<Suggestion> {
    let Some(d) = env.xdg_documents.as_ref() else {
        return Vec::new();
    };
    vec![Suggestion {
        detector_id: "xdg-documents",
        action: Action::AddKnowledgeSource {
            path: d.clone(),
            mode: SourceMode::Discover,
        },
        reason: format!(
            "XDG Documents at {} — register as discovery root so per-folder qdr.toml decides what's indexed",
            d.display()
        ),
    }]
}

fn probe_xdg_downloads(env: &ProbeEnv) -> Vec<Suggestion> {
    let Some(d) = env.xdg_download.as_ref() else {
        return Vec::new();
    };
    vec![Suggestion {
        detector_id: "xdg-downloads",
        action: Action::AddKnowledgeSource {
            path: d.clone(),
            mode: SourceMode::Discover,
        },
        reason: format!(
            "XDG Downloads at {} — register as discovery root (drop qdr.toml in subfolders worth indexing)",
            d.display()
        ),
    }]
}

fn probe_home_knowledge(env: &ProbeEnv) -> Vec<Suggestion> {
    // Register as a *discover root*, not a wholesale manifest — that way
    // nested `qdr.toml` files (e.g. dropped by `telegram-exports` inside
    // ~/knowledge/telegram) decide what's indexed under each subfolder,
    // and we don't compete with them on the same files.
    for name in &["knowledge", "Knowledge"] {
        let root = env.home.join(name);
        if !root.is_dir() {
            continue;
        }
        let any = std::fs::read_dir(&root)
            .ok()
            .map(|rd| rd.flatten().next().is_some())
            .unwrap_or(false);
        if !any {
            continue;
        }
        return vec![Suggestion {
            detector_id: "home-knowledge-dir",
            action: Action::AddKnowledgeSource {
                path: root.clone(),
                mode: SourceMode::Discover,
            },
            reason: format!(
                "Curated knowledge folder at {} — register as discovery root so per-subfolder qdr.toml decides what's indexed",
                root.display()
            ),
        }];
    }
    Vec::new()
}

fn probe_telegram_exports(env: &ProbeEnv) -> Vec<Suggestion> {
    // Default Telegram Desktop export dest: <DOWNLOAD>/Telegram Desktop/<DataExport_*>
    // Users also drop them into ~/knowledge or ~/Documents.
    let mut roots: Vec<PathBuf> = Vec::new();
    for r in [
        env.xdg_download.clone(),
        env.xdg_documents.clone(),
        Some(env.home.join("knowledge")),
        Some(env.home.join("Knowledge")),
    ]
    .into_iter()
    .flatten()
    {
        if r.is_dir() {
            roots.push(r);
        }
    }
    let mut found_folders: HashSet<PathBuf> = HashSet::new();
    for root in &roots {
        for dent in walk_capped(root, 4) {
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue,
            };
            let p = dent.path();
            // Match either ChatExport_* dir or `result.json` next to a
            // Telegram-shaped tree.
            let basename = match p.file_name().and_then(|n| n.to_str()) {
                Some(b) => b,
                None => continue,
            };
            if p.is_dir() && basename.starts_with("ChatExport_") {
                found_folders.insert(p.to_path_buf());
                continue;
            }
            if basename == "result.json" {
                if let Some(parent) = p.parent() {
                    // Heuristic: a Telegram export's `result.json` lives next
                    // to one of the media subfolders. If we see a media dir
                    // sibling, treat the parent as the export root.
                    let media_dir_present = ["photos", "video_files", "voice_messages"]
                        .iter()
                        .any(|n| parent.join(n).is_dir());
                    if media_dir_present {
                        found_folders.insert(parent.to_path_buf());
                    }
                }
            }
        }
    }
    // Dedupe: when one found folder is an ancestor of another, keep only
    // the ancestor (its manifest covers the nested ChatExport_* tree).
    let folder_list: Vec<PathBuf> = {
        let mut v: Vec<PathBuf> = found_folders.iter().cloned().collect();
        v.sort();
        let mut keep: Vec<PathBuf> = Vec::new();
        for f in v {
            if !keep.iter().any(|k| f.starts_with(k)) {
                keep.push(f);
            }
        }
        keep
    };

    let mut out: Vec<Suggestion> = Vec::new();
    for folder in &folder_list {
        let name = format!("telegram-{}", folder_basename(folder));
        out.push(drop_qdr_manifest(
            "telegram-exports",
            folder.clone(),
            ManifestTemplate {
                name,
                include: vec![
                    "**/messages*.html".into(),
                    "**/result.json".into(),
                    "**/*.txt".into(),
                ],
                exclude: vec![
                    "**/photos/**".into(),
                    "**/stickers/**".into(),
                    "**/voice_messages/**".into(),
                    "**/video_files/**".into(),
                    "**/round_video_messages/**".into(),
                    "**/files/**".into(),
                    "**/contacts/**".into(),
                    "**/css/**".into(),
                    "**/js/**".into(),
                    "**/images/**".into(),
                ],
                tags: vec!["telegram".into(), "chat".into()],
                max_depth: None,
                max_file_bytes: Some(100 * 1024 * 1024),
            },
            format!("Telegram chat export at {}", folder.display()),
        ));
    }

    // Hint: if Telegram Desktop is installed but we found no exports, tell
    // the user how to make one (live tdata is encrypted/binary).
    let telegram_installed = env.has_app("telegram") || env.has_app("org.telegram.desktop");
    if telegram_installed && found_folders.is_empty() {
        out.push(Suggestion {
            detector_id: "telegram-exports",
            action: Action::Hint {
                message:
                    "Telegram is installed but no chat exports were found. Live `tdata/` is \
                     encrypted — to make messages searchable, open Telegram → Settings → Advanced → \
                     Export Chat History (JSON or HTML)."
                        .into(),
            },
            reason: "Telegram detected but no exports".into(),
        });
    }
    out
}

fn probe_obsidian(env: &ProbeEnv) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in note_probe_roots(env) {
        for dent in walk_capped(&root, 4) {
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue,
            };
            let p = dent.path();
            if p.file_name().and_then(|n| n.to_str()) == Some(".obsidian")
                && dent.file_type().map_or(false, |t| t.is_dir())
            {
                if let Some(parent) = p.parent() {
                    let folder = parent.to_path_buf();
                    if !seen.insert(folder.clone()) {
                        continue;
                    }
                    let name = folder_basename(&folder);
                    out.push(drop_qdr_manifest(
                        "obsidian-vaults",
                        folder.clone(),
                        ManifestTemplate {
                            name: format!("obsidian-{name}"),
                            include: vec!["**/*.md".into(), "**/*.canvas".into()],
                            exclude: vec![".obsidian/**".into(), ".trash/**".into()],
                            tags: vec!["obsidian".into()],
                            max_depth: None,
                            max_file_bytes: None,
                        },
                        format!("Obsidian vault at {}", folder.display()),
                    ));
                }
            }
        }
    }
    out
}

fn probe_logseq(env: &ProbeEnv) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in note_probe_roots(env) {
        for dent in walk_capped(&root, 4) {
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue,
            };
            let p = dent.path();
            if p.file_name().and_then(|n| n.to_str()) == Some("logseq")
                && dent.file_type().map_or(false, |t| t.is_dir())
            {
                let parent = match p.parent() {
                    Some(p) => p.to_path_buf(),
                    None => continue,
                };
                if !parent.join("pages").is_dir() {
                    continue;
                }
                if !seen.insert(parent.clone()) {
                    continue;
                }
                let name = folder_basename(&parent);
                out.push(drop_qdr_manifest(
                    "logseq-graphs",
                    parent.clone(),
                    ManifestTemplate {
                        name: format!("logseq-{name}"),
                        include: vec![
                            "pages/**/*.md".into(),
                            "journals/**/*.md".into(),
                            "assets/**/*.md".into(),
                        ],
                        exclude: vec!["logseq/**".into(), "**/.recycle/**".into()],
                        tags: vec!["logseq".into()],
                        max_depth: None,
                        max_file_bytes: None,
                    },
                    format!("Logseq graph at {}", parent.display()),
                ));
            }
        }
    }
    out
}

fn probe_joplin(env: &ProbeEnv) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let candidates = [
        env.home.join("Joplin"),
        env.xdg_documents
            .clone()
            .map(|d| d.join("Joplin"))
            .unwrap_or_default(),
    ];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for c in candidates {
        if !c.is_dir() {
            continue;
        }
        if !seen.insert(c.clone()) {
            continue;
        }
        out.push(drop_qdr_manifest(
            "joplin-export",
            c.clone(),
            ManifestTemplate {
                name: "joplin".into(),
                include: vec!["**/*.md".into()],
                exclude: vec![],
                tags: vec!["joplin".into()],
                max_depth: None,
                max_file_bytes: None,
            },
            format!("Joplin notes folder at {}", c.display()),
        ));
    }
    out
}

fn probe_zettlr(env: &ProbeEnv) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in note_probe_roots(env) {
        for dent in walk_capped(&root, 4) {
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue,
            };
            let p = dent.path();
            if p.file_name().and_then(|n| n.to_str()) == Some(".zettlr-roots") && p.is_file() {
                let parent = match p.parent() {
                    Some(p) => p.to_path_buf(),
                    None => continue,
                };
                if !seen.insert(parent.clone()) {
                    continue;
                }
                out.push(drop_qdr_manifest(
                    "zettlr-projects",
                    parent.clone(),
                    ManifestTemplate {
                        name: format!("zettlr-{}", folder_basename(&parent)),
                        include: vec!["**/*.md".into()],
                        exclude: vec![],
                        tags: vec!["zettlr".into()],
                        max_depth: None,
                        max_file_bytes: None,
                    },
                    format!("Zettlr project at {}", parent.display()),
                ));
            }
        }
    }
    out
}

fn probe_bruno(env: &ProbeEnv) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut roots: Vec<PathBuf> = Vec::new();
    for r in [
        env.home.join(".var/app/com.usebruno.Bruno"),
        env.home.join("Bruno"),
        env.home.join("bruno"),
    ] {
        if r.is_dir() {
            roots.push(r);
        }
    }
    if roots.is_empty() && !env.has_app("bruno") {
        return out;
    }
    // Look for any directory that has *.bru files; treat its top-most
    // ancestor with .bru content as the collection root.
    for root in &roots {
        for dent in walk_capped(root, 5) {
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue,
            };
            let p = dent.path();
            if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("bru") {
                let parent = match p.parent() {
                    Some(p) => p.to_path_buf(),
                    None => continue,
                };
                if !seen.insert(parent.clone()) {
                    continue;
                }
                out.push(drop_qdr_manifest(
                    "bruno-collections",
                    parent.clone(),
                    ManifestTemplate {
                        name: format!("bruno-{}", folder_basename(&parent)),
                        include: vec!["**/*.bru".into(), "**/*.json".into()],
                        exclude: vec![],
                        tags: vec!["bruno".into(), "api".into()],
                        max_depth: None,
                        max_file_bytes: None,
                    },
                    format!("Bruno collection at {}", parent.display()),
                ));
                break; // first .bru per root is enough
            }
        }
    }
    out
}

fn probe_agent_histories(env: &ProbeEnv) -> Vec<Suggestion> {
    let mut out = Vec::new();
    // Strict secret excludes — applied to every agent dir we touch.
    let common_excludes = vec![
        "**/credentials*".into(),
        "**/*token*".into(),
        "**/oauth*".into(),
        "**/auth.json".into(),
        "**/*.key".into(),
        "**/.credentials.json".into(),
        "**/.env*".into(),
        "**/*.pem".into(),
        "**/*.p12".into(),
        // Caches and binary blobs — large and pointless to embed.
        "**/cache/**".into(),
        "**/Cache/**".into(),
        "**/CachedData/**".into(),
        "**/Code Cache/**".into(),
        "**/GPUCache/**".into(),
        "**/blob_storage/**".into(),
        "**/Service Worker/**".into(),
        "**/Crashpad/**".into(),
        // Recursive search-cache: tool_results from MCP calls into our
        // own `sy knowledge mcp` get logged here as JSON-encoded strings
        // that contain prior chunk_text. Indexing them creates a feedback
        // loop where searches surface cached search results.
        "**/tool-results/mcp-sy-knowledge-*".into(),
        "**/tool_results/mcp-sy-knowledge-*".into(),
    ];
    for (rel, label) in &[
        (".claude", "claude"),
        (".codex", "codex"),
        (".gemini", "gemini"),
        (".cursor", "cursor"),
        (".antigravity", "antigravity"),
        (".agents", "agents"),
    ] {
        let folder = env.home.join(rel);
        if !folder.is_dir() {
            continue;
        }
        out.push(drop_qdr_manifest(
            "agent-histories",
            folder.clone(),
            ManifestTemplate {
                name: format!("agent-{label}"),
                include: vec![
                    "**/*.md".into(),
                    "**/*.jsonl".into(),
                    "**/*.txt".into(),
                    // Capture file-history HTML (Claude) but not arbitrary html.
                    "file-history/**/*.html".into(),
                    // Agents tend to keep skills/ as markdown.
                    "skills/**/*.md".into(),
                    "sessions/**/*.json".into(),
                    "history/**/*.jsonl".into(),
                    "history/**/*.json".into(),
                ],
                exclude: common_excludes.clone(),
                tags: vec!["agent".into(), (*label).into()],
                max_depth: None,
                // Agent histories grow large; cap at 5 MB per file.
                max_file_bytes: Some(5 * 1024 * 1024),
            },
            format!("AI-agent history at {}", folder.display()),
        ));
    }
    out
}

fn probe_dotfiles(env: &ProbeEnv) -> Vec<Suggestion> {
    let mut out = Vec::new();
    for name in &["dotfiles", ".dotfiles"] {
        let p = env.home.join(name);
        if !p.is_dir() {
            continue;
        }
        out.push(drop_qdr_manifest(
            "dotfiles",
            p.clone(),
            ManifestTemplate {
                name: format!("dotfiles-{name}"),
                include: vec![
                    "**/*.md".into(),
                    "**/*.toml".into(),
                    "**/*.yaml".into(),
                    "**/*.yml".into(),
                    "**/*.conf".into(),
                    "**/*.config".into(),
                    "**/*.sh".into(),
                ],
                exclude: vec![
                    "**/secrets*".into(),
                    "**/.env*".into(),
                    "**/*.key".into(),
                    "**/*token*".into(),
                ],
                tags: vec!["dotfiles".into()],
                max_depth: None,
                max_file_bytes: Some(2 * 1024 * 1024),
            },
            format!("Dotfiles repo at {}", p.display()),
        ));
    }
    out
}

// ─── MCP wiring detectors ─────────────────────────────────────────────
//
// One detector per agent. Each one:
//   - Returns empty when the agent's home dir / config file is absent
//     (don't propose creating configs for tools the user hasn't
//     installed).
//   - Returns a `RemoveMcpServer` suggestion when `[knowledge].mcp_enabled`
//     is `false` and the entry is currently registered.
//   - Returns an `AddMcpServer` suggestion otherwise. The apply step is
//     idempotent — if the existing entry already matches what we'd
//     write, the apply reports `= already wired`.
//
// `mcp-antigravity` and `mcp-agents` ship as off-by-default hint-only
// detectors because no canonical MCP config path is verified for them.

use crate::auto_mcp::{self, McpAgent};
use crate::knowledge::sources;

fn mcp_suggestion(agent: McpAgent, _env: &ProbeEnv) -> Vec<Suggestion> {
    let detector_id = match agent {
        McpAgent::Claude => "mcp-claude",
        McpAgent::Cursor => "mcp-cursor",
        McpAgent::Codex => "mcp-codex",
        McpAgent::Gemini => "mcp-gemini",
        McpAgent::Goose => "mcp-goose",
        McpAgent::Antigravity => "mcp-antigravity",
        McpAgent::Agents => "mcp-agents",
    };
    let Some(state) = auto_mcp::read_state(agent) else {
        return Vec::new();
    };

    if !state.writable {
        // Hint-only — flag the agent's presence so users discover the
        // detector via `sy auto list-detectors`, but don't write blind.
        return vec![Suggestion {
            detector_id,
            action: Action::Hint {
                message: format!(
                    "{} detected at {} — no canonical MCP config path is supported yet. \
                     Drop a writer in src/auto_mcp.rs once the schema is confirmed.",
                    agent.label(),
                    state.path.display()
                ),
            },
            reason: format!("{} present, no writable MCP config", agent.label()),
        }];
    }

    let desired = auto_mcp::desired_entry();

    if !sources::mcp_enabled() {
        return if state.registered.is_some() {
            vec![Suggestion {
                detector_id,
                action: Action::RemoveMcpServer {
                    agent,
                    name: auto_mcp::SERVER_NAME.into(),
                },
                reason: format!(
                    "[knowledge].mcp_enabled = false; remove sy-knowledge from {}",
                    agent.label()
                ),
            }]
        } else {
            Vec::new()
        };
    }

    let already_matches = match &state.registered {
        Some(e) => e.command == desired.command && e.args == desired.args,
        None => false,
    };
    if already_matches {
        return Vec::new();
    }
    let reason = match &state.registered {
        Some(_) => format!("update sy-knowledge entry in {}", agent.label()),
        None => format!("register sy-knowledge in {}", agent.label()),
    };
    vec![Suggestion {
        detector_id,
        action: Action::AddMcpServer {
            agent,
            name: auto_mcp::SERVER_NAME.into(),
            command: desired.command,
            args: desired.args,
        },
        reason,
    }]
}

fn probe_mcp_claude(env: &ProbeEnv) -> Vec<Suggestion> {
    mcp_suggestion(McpAgent::Claude, env)
}

fn probe_mcp_cursor(env: &ProbeEnv) -> Vec<Suggestion> {
    mcp_suggestion(McpAgent::Cursor, env)
}

fn probe_mcp_codex(env: &ProbeEnv) -> Vec<Suggestion> {
    mcp_suggestion(McpAgent::Codex, env)
}

fn probe_mcp_gemini(env: &ProbeEnv) -> Vec<Suggestion> {
    mcp_suggestion(McpAgent::Gemini, env)
}

fn probe_mcp_goose(env: &ProbeEnv) -> Vec<Suggestion> {
    mcp_suggestion(McpAgent::Goose, env)
}

fn probe_mcp_antigravity(env: &ProbeEnv) -> Vec<Suggestion> {
    mcp_suggestion(McpAgent::Antigravity, env)
}

fn probe_mcp_agents(env: &ProbeEnv) -> Vec<Suggestion> {
    mcp_suggestion(McpAgent::Agents, env)
}
