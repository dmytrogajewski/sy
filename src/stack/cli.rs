//! `sy stack <push|pop|list|preview|remove|move|link|toggle|action>` impls.

use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use super::{
    ipc,
    state::{self, Item},
    Kind,
};

const MAX_APP_DEFAULT: usize = 8;
const MAX_USER_DEFAULT: usize = 8;

pub fn push(
    item: &str,
    kind_str: &str,
    name: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let kind = Kind::parse(kind_str)?;
    let new_item: Item = if item == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("read stdin")?;
        let display_name = name
            .map(str::to_string)
            .unwrap_or_else(|| {
                let head = String::from_utf8_lossy(&buf);
                head.lines()
                    .next()
                    .unwrap_or("(content)")
                    .chars()
                    .take(40)
                    .collect()
            });
        let content_kind = if std::str::from_utf8(&buf).is_ok() {
            "text"
        } else {
            "binary"
        };
        if dry_run {
            eprintln!(
                "would push (dry-run): kind={} name={} bytes={} content_kind={}",
                kind.as_str(),
                display_name,
                buf.len(),
                content_kind
            );
            return Ok(());
        }
        state::push_content(kind, display_name, &buf, content_kind)?
    } else {
        let p = Path::new(item);
        if !p.exists() {
            anyhow::bail!("path not found: {}", p.display());
        }
        let display_name = name
            .map(str::to_string)
            .unwrap_or_else(|| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("(file)")
                    .to_string()
            });
        if dry_run {
            eprintln!(
                "would push (dry-run): kind={} name={} path={}",
                kind.as_str(),
                display_name,
                p.display()
            );
            return Ok(());
        }
        state::push_file(kind, display_name, p)?
    };

    let id = new_item.id.clone();
    let mut items = state::load()?;
    items.items.push(new_item);
    state::check_caps(&mut items, MAX_APP_DEFAULT, MAX_USER_DEFAULT);
    state::save(&items)?;

    let _ = ipc::send(&ipc::Op::Refresh);

    if json {
        println!(r#"{{"id":"{id}"}}"#);
    } else {
        println!("{id}");
    }
    Ok(())
}

pub fn pop(kind_str: &str, id: Option<&str>) -> Result<()> {
    let kind = Kind::parse(kind_str)?;
    let mut items = state::load()?;

    let target_idx = match id {
        Some(id) => items
            .items
            .iter()
            .position(|i| i.id == id)
            .ok_or_else(|| state::not_found(id))?,
        None => {
            // Top = newest of this kind. Within a tied second, prefer the
            // later-inserted item (higher index in items.items).
            let mut best: Option<(u64, usize)> = None;
            for (idx, it) in items.items.iter().enumerate() {
                if it.kind != kind {
                    continue;
                }
                let take = match best {
                    None => true,
                    Some((t, _)) => it.created_at >= t,
                };
                if take {
                    best = Some((it.created_at, idx));
                }
            }
            match best {
                Some((_, idx)) => idx,
                None => {
                    anyhow::bail!("stack pool '{}' is empty", kind.as_str());
                }
            }
        }
    };

    let popped = items.items.remove(target_idx);
    state::delete_blobs(&popped.id);
    state::save(&items)?;
    let _ = ipc::send(&ipc::Op::Refresh);
    println!("{}", popped.id);
    Ok(())
}

pub fn list(json: bool) -> Result<()> {
    let items = state::load()?;
    if json {
        // Stable schema documented in plan.
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    if items.items.is_empty() {
        println!("(stack is empty)");
        return Ok(());
    }
    println!(
        "{:<10} {:<5} {:<8} {:<12} {}",
        "ID", "KIND", "TYPE", "SIZE", "NAME"
    );
    let mut sorted = items.items.clone();
    sorted.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    for i in sorted {
        println!(
            "{:<10} {:<5} {:<8} {:<12} {}",
            i.id,
            i.kind.as_str(),
            i.content_kind,
            human_bytes(i.size),
            i.name
        );
    }
    Ok(())
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut x = n as f64;
    let mut u = 0;
    while x >= 1024.0 && u + 1 < UNITS.len() {
        x /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", n, UNITS[u])
    } else {
        format!("{:.1} {}", x, UNITS[u])
    }
}

pub fn preview(id: &str) -> Result<()> {
    let items = state::load()?;
    let it = state::find(&items, id)?.clone();
    if let Some(p) = &it.path {
        let bytes = fs::read(p).with_context(|| format!("read {}", p.display()))?;
        std::io::Write::write_all(&mut std::io::stdout(), &bytes)?;
    } else {
        let bytes = state::read_payload(&it.id)?;
        std::io::Write::write_all(&mut std::io::stdout(), &bytes)?;
    }
    Ok(())
}

pub fn remove(id: &str) -> Result<()> {
    let mut items = state::load()?;
    let idx = items
        .items
        .iter()
        .position(|i| i.id == id)
        .ok_or_else(|| state::not_found(id))?;
    let it = items.items.remove(idx);
    state::delete_blobs(&it.id);
    state::save(&items)?;
    let _ = ipc::send(&ipc::Op::Refresh);
    Ok(())
}

pub fn move_to(id: &str, dest: &Path) -> Result<()> {
    let mut items = state::load()?;
    let idx = items
        .items
        .iter()
        .position(|i| i.id == id)
        .ok_or_else(|| state::not_found(id))?;
    let it = items.items[idx].clone();
    fs::create_dir_all(dest).with_context(|| format!("mkdir {}", dest.display()))?;
    let target_name = sanitize_name(&it.name);
    let target = dest.join(&target_name);

    if let Some(src) = &it.path {
        // For file items, prefer rename (keeps inode); fall back to copy+remove.
        if fs::rename(src, &target).is_err() {
            fs::copy(src, &target).with_context(|| format!("copy {}", src.display()))?;
            let _ = fs::remove_file(src);
        }
    } else {
        let bytes = state::read_payload(&it.id)?;
        fs::write(&target, &bytes).with_context(|| format!("write {}", target.display()))?;
    }

    items.items.remove(idx);
    state::delete_blobs(&it.id);
    state::save(&items)?;
    let _ = ipc::send(&ipc::Op::Refresh);
    println!("{}", target.display());
    Ok(())
}

fn sanitize_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("item");
    }
    out
}

pub fn link(id: &str) -> Result<()> {
    let items = state::load()?;
    let it = state::find(&items, id)?;
    let p = state::link_path(it)?;
    println!("{}", p.display());
    Ok(())
}

/// Same as `link` but also writes the path to wl-copy and posts a desktop
/// notification. Used by the right-click "link" action so the path doesn't
/// vanish into a detached child's stdout.
fn link_and_copy(id: &str) -> Result<()> {
    use std::io::Write;
    let items = state::load()?;
    let it = state::find(&items, id)?;
    let p = state::link_path(it)?;
    let s = p.display().to_string();
    let mut child = Command::new("wl-copy").stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(s.as_bytes())?;
    }
    let _ = child.wait();
    notify("link copied", &s);
    Ok(())
}

/// Post a desktop notification (no-op if `notify-send` is missing).
fn notify(title: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-t", "2500", "-a", "sy-stack", title, body])
        .spawn();
}

fn notify_err(action: &str, e: &anyhow::Error) {
    notify(&format!("sy stack: {action} failed"), &e.to_string());
}

/// Bar-side context-menu dispatch. The bar process resolves slot → id and
/// passes a `source` hint (stack | clip) so we know which backing store
/// owns the id.
pub fn action(id: &str, action: &str, source: &str) -> Result<()> {
    match source {
        "clip" => clip_action(id, action),
        _ => stack_action(id, action),
    }
}

fn stack_action(id: &str, action: &str) -> Result<()> {
    let result = match action {
        "copy" => copy_item(id),
        "preview" => preview_in_window(id),
        "remove" => remove(id),
        "link" => link_and_copy(id),
        "move" => move_via_picker(id),
        "onto" => onto_via_picker(id),
        "agent" => agent_via_picker(id),
        other => Err(anyhow::anyhow!("unknown action: {other}")),
    };
    if let Err(e) = &result {
        notify_err(action, e);
    }
    result
}

fn clip_action(id: &str, action: &str) -> Result<()> {
    match action {
        "copy" => super::clip::copy_to_clipboard(id),
        "preview" => clip_preview(id),
        "remove" => clip_remove(id),
        other => Err(anyhow::anyhow!("unknown clip action: {other}")),
    }
}

/// Decode the clipboard entry to a stable temp file under
/// `$XDG_RUNTIME_DIR/sy/clip/<id>` and open it with the user's default app.
/// We sniff the bytes for image magic so the file gets a sensible
/// extension (xdg-open dispatches by mime/extension).
fn clip_preview(id: &str) -> Result<()> {
    let bytes = super::clip::decode(id)?;
    let dir = clip_cache_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let ext = sniff_ext(&bytes);
    let path = dir.join(format!("{id}{ext}"));
    if !path.exists() {
        fs::write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
    }
    let _ = Command::new("xdg-open").arg(&path).spawn();
    Ok(())
}

fn clip_cache_dir() -> Result<std::path::PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    Ok(base.join("sy").join("clip"))
}

/// Best-effort extension sniffer for clipboard payloads. Most are text;
/// detect the few common image magics so `xdg-open` routes to a viewer.
fn sniff_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ".png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        ".jpg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        ".gif"
    } else if bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        ".webp"
    } else if std::str::from_utf8(bytes).is_ok() {
        ".txt"
    } else {
        ".bin"
    }
}

fn clip_remove(id: &str) -> Result<()> {
    let _ = Command::new("cliphist").args(["delete-query", id]).status();
    let _ = ipc::send(&ipc::Op::Refresh);
    Ok(())
}

/// Open the item in the user's default app via xdg-open. For file items
/// that's the file itself; for content items we materialise a stable temp
/// file via `state::link_path` (same path the `link` action gives) and open
/// that. Detached so the bar daemon doesn't block.
fn preview_in_window(id: &str) -> Result<()> {
    let items = state::load()?;
    let it = state::find(&items, id)?;
    let p = state::link_path(it)?;
    let _ = Command::new("xdg-open").arg(&p).spawn();
    Ok(())
}

fn copy_item(id: &str) -> Result<()> {
    use std::io::Write;
    let items = state::load()?;
    let it = state::find(&items, id)?;
    let bytes = if let Some(p) = &it.path {
        fs::read(p)?
    } else {
        state::read_payload(&it.id)?
    };
    let mut child = Command::new("wl-copy").stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&bytes)?;
    }
    let _ = child.wait();
    Ok(())
}

fn move_via_picker(id: &str) -> Result<()> {
    // Offer common destinations + a free-form input. fuzzel --dmenu accepts
    // typed text not present in the list, so the suggestions are non-binding.
    let home = std::env::var("HOME").unwrap_or_default();
    let suggestions: Vec<String> = [
        format!("{home}/Downloads"),
        format!("{home}/Documents"),
        format!("{home}/Desktop"),
        "/tmp".into(),
    ]
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect();
    let dest = fuzzel_pick("move to", &suggestions)?;
    let dest = dest.trim();
    if dest.is_empty() {
        return Ok(());
    }
    let p = Path::new(dest);
    let result = move_to(id, p);
    if result.is_ok() {
        notify("moved", &format!("→ {}", p.display()));
    }
    result
}

fn onto_via_picker(id: &str) -> Result<()> {
    let names = super::onto::list_names().unwrap_or_default();
    if names.is_empty() {
        return Err(anyhow::anyhow!(
            "no [[stack.onto]] integrations configured — add some to sy.toml and `sy apply`"
        ));
    }
    let pick = fuzzel_pick("onto", &names)?;
    if pick.is_empty() {
        return Ok(());
    }
    let result = super::onto::run(&pick, id);
    if result.is_ok() {
        notify("onto", &pick);
    }
    result
}

fn agent_via_picker(id: &str) -> Result<()> {
    // 1) pick agent  2) ask for prompt text  3) spawn `sy agt run` with the
    // user's prompt + a context line pointing at the materialised file.
    let agents = crate::agt::registry::load().unwrap_or_default();
    if agents.is_empty() {
        return Err(anyhow::anyhow!("no agents registered (configs/sy/agents.toml)"));
    }
    let names: Vec<String> = agents.iter().map(|a| a.name.clone()).collect();
    let pick = fuzzel_pick("agent", &names)?;
    if pick.is_empty() {
        return Ok(());
    }

    let items = state::load()?;
    let it = state::find(&items, id)?;
    let path = state::link_path(it)?;

    let user_prompt = fuzzel_prompt(&format!("prompt for {pick}"))?;
    let user_prompt = user_prompt.trim();
    if user_prompt.is_empty() {
        return Ok(());
    }

    let combined = format!("{user_prompt}\n\nContext: {}", path.display());
    let _ = Command::new("sy")
        .args(["agt", "run", "--agent", &pick, &combined])
        .spawn();
    notify("agent", &format!("{pick}: {user_prompt}"));
    Ok(())
}

fn fuzzel_prompt(prompt: &str) -> Result<String> {
    let out = Command::new("fuzzel")
        .args(["--dmenu", "--prompt"])
        .arg(format!("{prompt} » "))
        .stdin(Stdio::null())
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn fuzzel_pick(prompt: &str, options: &[String]) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("fuzzel")
        .args(["--dmenu", "--prompt"])
        .arg(format!("{prompt} » "))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        for o in options {
            writeln!(stdin, "{o}")?;
        }
    }
    let out = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn toggle() -> Result<()> {
    // Tries socket first; if the daemon isn't listening, that's a no-op.
    // No PID-fallback for now — `sy stack bar` is a single foreground
    // process and users can manage its lifecycle via niri's spawn-at-startup.
    ipc::send(&ipc::Op::Toggle)
}
