use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use minijinja::Environment;
use serde::Deserialize;
use walkdir::WalkDir;

mod agt;
mod aiplane;
mod auto;
mod auto_mcp;
mod bat;
mod bright;
mod bt;
mod cal;
mod disk;
mod fido;
mod gpu;
mod knowledge;
mod net;
mod notif;
mod npu;
mod popup;
mod pwr;
mod silent;
mod sound;
mod stack;
mod vol;
mod wallpaper;
mod wifi;

/// sy — apply niri rice configs with themes and templating.
#[derive(Parser)]
#[command(name = "sy", version, about, long_about = None)]
struct Cli {
    /// Override the repo root (otherwise walk up from cwd)
    #[arg(long, global = true, env = "SY_ROOT")]
    root: Option<PathBuf>,

    /// Override the target directory (default: $XDG_CONFIG_HOME or ~/.config)
    #[arg(long, global = true)]
    target: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render all templates under configs/ and write them to the target
    Apply {
        /// Theme name (resolved as themes/<name>.toml)
        #[arg(short, long)]
        theme: Option<String>,
        /// Don't write anything — print what would change
        #[arg(long)]
        dry_run: bool,
    },
    /// List available themes under themes/
    Themes,
    /// Render a single template to stdout
    Render {
        /// Theme to render with
        #[arg(short, long)]
        theme: Option<String>,
        /// Path relative to configs/, e.g. waybar/style.css
        path: PathBuf,
    },
    /// AI-agent subsystem (ACP-driven sessions managed by sy-agentd)
    Agt {
        #[command(subcommand)]
        sub: agt::AgtCmd,
    },
    /// Toggle a named popup window (e.g. `sy popup agents`, `sy popup cal`, `sy popup nmtui`)
    Popup {
        /// Popup identifier (agents|cal|nmtui)
        key: String,
    },
    /// Interactive terminal calendar (h/l prev/next month, j/k year, t today, q quit)
    Cal,
    /// Fuzzel-based wifi picker via nmcli
    Wifi,
    /// Fuzzel-based network dropdown (wifi, VPN, toggles, nmtui)
    Net,
    /// Copy the running sy binary into ~/.local/bin (real file, SELinux-safe)
    Install,
    /// Open the rendered Telegram palette in Telegram Desktop to apply it
    TgTheme,
    /// Set the desktop wallpaper (swaybg). With no path, prints the current one.
    Wallpaper {
        /// Path to an image file. Omit to print the current wallpaper.
        path: Option<PathBuf>,
        /// Re-spawn swaybg from the saved state (used by niri startup).
        #[arg(long, conflicts_with_all = ["path", "default"])]
        start: bool,
        /// Apply the built-in default (kitten logo centered on black) and
        /// clear the saved state so it persists across reboots.
        #[arg(long, conflicts_with_all = ["path", "start"])]
        default: bool,
    },
    /// Adjust volume via wpctl (up|down|mute|mic-mute|pick) or emit waybar JSON.
    Vol {
        /// up | down | mute | mic-mute | pick
        action: Option<String>,
        /// Emit waybar-compatible JSON instead of acting.
        #[arg(long, conflicts_with = "action")]
        waybar: bool,
    },
    /// Adjust display brightness via brightnessctl (up|down) or emit waybar JSON.
    Bright {
        /// up | down
        action: Option<String>,
        /// Emit waybar-compatible JSON instead of acting.
        #[arg(long, conflicts_with = "action")]
        waybar: bool,
    },
    /// Battery applet — emits waybar JSON keyed to charge level / charging state.
    Bat {
        /// Emit waybar-compatible JSON.
        #[arg(long)]
        waybar: bool,
    },
    /// NVIDIA GPU applet — bar tile showing VRAM pressure + util.
    /// `--waybar` emits JSON; no args prints a human-readable summary.
    Gpu {
        #[arg(long)]
        waybar: bool,
    },
    /// AMD Ryzen AI NPU applet — bar tile showing active/idle + holders.
    /// `--waybar` emits JSON; no args prints a human-readable summary.
    Npu {
        #[arg(long)]
        waybar: bool,
    },
    /// Disk applet — bar tile when free space on `/` is below the threshold;
    /// no args opens a fuzzel cleanup picker.
    Disk {
        /// Emit waybar-compatible JSON.
        #[arg(long)]
        waybar: bool,
        /// Override the low-space threshold (default 30, env SY_DISK_THRESHOLD_GIB).
        #[arg(long = "threshold-gib", value_name = "GIB")]
        threshold_gib: Option<u64>,
    },
    /// Notification watcher/counter (watch|waybar|count|clear)
    Notif {
        /// watch | waybar | count | clear | menu | list | show | read.
        /// Omit to open the fuzzel menu (same as `menu`).
        action: Option<String>,
        /// Trailing args (e.g. an id for `read`/`show`, flags for `list`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    /// Bluetooth menu / status (fuzzel dropdown; --waybar for bar JSON)
    Bt {
        /// Emit waybar-compatible JSON
        #[arg(long)]
        waybar: bool,
    },
    /// Power menu: tuned profile + lock/suspend/reboot/shutdown/logout
    Pwr {
        /// Emit waybar-compatible JSON
        #[arg(long)]
        waybar: bool,
    },
    /// FIDO/U2F auth for swaylock via pam_u2f (enable|disable|status)
    Fido {
        /// enable | disable | status (default: status)
        action: Option<String>,
    },
    /// Silent hours — quiet output during a configurable time window.
    Silent {
        /// toggle (default) | enable | disable | auto | status | watch
        action: Option<String>,
        /// Emit waybar-compatible JSON.
        #[arg(long, conflicts_with = "action")]
        waybar: bool,
    },
    /// Session sound jingles (login/logout). Silent-gated.
    Snd {
        /// login | logout | test
        action: String,
    },
    /// Temporary-artifact stack bar (clip / app / user pools)
    Stack {
        #[command(subcommand)]
        sub: stack::StackCmd,
    },
    /// System-wide semantic-search knowledge plane (Qdrant + fastembed)
    Knowledge {
        #[command(subcommand)]
        sub: knowledge::KnowledgeCmd,
    },
    /// Multi-workload NPU plane: list / status / run any registered
    /// workload (embed, rerank, vad, stt, ocr, …). Knowledge plane
    /// consumes this; future workloads register through the same registry.
    Aiplane {
        #[command(subcommand)]
        sub: aiplane::cli::AiplaneCmd,
    },
    /// Probe the system and propose opinionated defaults (knowledge sources,
    /// qdr.toml manifests). Dry-run by default; pass `--apply` to commit.
    Auto {
        #[command(subcommand)]
        sub: auto::AutoCmd,
    },
}

#[derive(Deserialize, Default)]
struct SyFile {
    theme: Option<String>,
}

const DEFAULT_THEME: &str = "gruvbox-material";

fn main() -> Result<()> {
    // Must run before any threads spawn: probes for /opt/AMD/ryzenai
    // and re-execs with LD_LIBRARY_PATH + ORT_DYLIB_PATH + the Ryzen AI
    // activate env vars baked in. No-op when the AMD venv isn't present
    // or when we've already re-execed.
    aiplane::reexec::maybe_reexec_with_amd_env();

    let result = run();
    match &result {
        Err(e) => {
            // Map AgtError / StackError to their declared exit codes
            // (CLIG: stable exit codes). For other errors fall through to
            // anyhow's default formatting.
            if let Some(ae) = e.downcast_ref::<agt::AgtError>() {
                eprintln!("error: {}", ae.msg);
                std::process::exit(ae.code);
            }
            if let Some(se) = e.downcast_ref::<stack::StackError>() {
                eprintln!("error: {}", se.msg);
                std::process::exit(se.code);
            }
            if let Some(ke) = e.downcast_ref::<knowledge::KnowledgeError>() {
                eprintln!("error: {}", ke.msg);
                std::process::exit(ke.code);
            }
        }
        Ok(()) => {}
    }
    result
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // Commands that render templates need the repo root + sy.toml; all
    // others (agents, popup, wifi, net, install) run without repo context
    // so they work even when cwd is outside the sy tree (e.g. when waybar
    // is spawned at startup with cwd=~).
    let resolve_repo = || -> Result<(PathBuf, SyFile, PathBuf)> {
        let root = match &cli.root {
            Some(p) => p.clone(),
            None => find_root().context(
                "could not find repo root — expected a parent directory containing both configs/ and themes/",
            )?,
        };
        let syf = load_sy_file(&root)?;
        let target = cli.target.clone().map(Ok).unwrap_or_else(default_target)?;
        Ok((root, syf, target))
    };

    match cli.command {
        Cmd::Apply { theme, dry_run } => {
            let (root, syf, target) = resolve_repo()?;
            let name = theme
                .or(syf.theme.clone())
                .unwrap_or_else(|| DEFAULT_THEME.to_string());
            let ctx = load_theme(&root, &name)?;
            apply(&root, &target, &ctx, &name, dry_run)
        }
        Cmd::Themes => {
            let (root, _, _) = resolve_repo()?;
            list_themes(&root)
        }
        Cmd::Render { theme, path } => {
            let (root, syf, _) = resolve_repo()?;
            let name = theme
                .or(syf.theme.clone())
                .unwrap_or_else(|| DEFAULT_THEME.to_string());
            let ctx = load_theme(&root, &name)?;
            let full = root.join("configs").join(&path);
            let content =
                fs::read_to_string(&full).with_context(|| format!("read {}", full.display()))?;
            let env = Environment::new();
            let rendered = env
                .render_str(&content, &ctx)
                .with_context(|| format!("render {}", path.display()))?;
            print!("{rendered}");
            Ok(())
        }
        Cmd::Agt { sub } => agt::dispatch(sub),
        Cmd::Popup { key } => popup::toggle(&key),
        Cmd::Cal => cal::run(),
        Cmd::Wifi => wifi::pick(),
        Cmd::Net => net::menu(),
        Cmd::Install => install(false),
        Cmd::TgTheme => tg_theme(),
        Cmd::Wallpaper {
            path,
            start,
            default,
        } => wallpaper::run(path, start, default),
        Cmd::Vol { action, waybar } => vol::run(action.as_deref(), waybar),
        Cmd::Bright { action, waybar } => bright::run(action.as_deref(), waybar),
        Cmd::Bat { waybar } => bat::run(waybar),
        Cmd::Gpu { waybar } => gpu::run(waybar),
        Cmd::Npu { waybar } => npu::run(waybar),
        Cmd::Disk {
            waybar,
            threshold_gib,
        } => disk::run(waybar, threshold_gib),
        Cmd::Notif { action, rest } => {
            let act = action.as_deref().unwrap_or("menu");
            notif::run(act, &rest)
        }
        Cmd::Bt { waybar } => bt::run(waybar),
        Cmd::Pwr { waybar } => pwr::run(waybar),
        Cmd::Fido { action } => fido::run(action.as_deref()),
        Cmd::Silent { action, waybar } => silent::run(action.as_deref(), waybar),
        Cmd::Snd { action } => match action.as_str() {
            "login" => sound::login(),
            "logout" => sound::logout(),
            "test" => sound::test(),
            other => Err(anyhow!("unknown snd action: {other} (login|logout|test)")),
        },
        Cmd::Stack { sub } => stack::dispatch(sub),
        Cmd::Knowledge { sub } => knowledge::dispatch(sub),
        Cmd::Aiplane { sub } => aiplane::cli::dispatch(sub),
        Cmd::Auto { sub } => auto::dispatch(sub),
    }
}

fn find_root() -> Result<PathBuf> {
    let mut cur = env::current_dir().context("cwd")?;
    loop {
        if cur.join("configs").is_dir() && cur.join("themes").is_dir() {
            return Ok(cur);
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => {
                return Err(anyhow!(
                    "reached filesystem root without finding configs/ + themes/"
                ))
            }
        }
    }
}

fn default_target() -> Result<PathBuf> {
    if let Ok(x) = env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Ok(PathBuf::from(x));
        }
    }
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config"))
}

fn load_sy_file(root: &Path) -> Result<SyFile> {
    let p = root.join("sy.toml");
    if !p.exists() {
        return Ok(SyFile::default());
    }
    let s = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    toml::from_str(&s).with_context(|| format!("parse {}", p.display()))
}

fn load_theme(root: &Path, name: &str) -> Result<toml::Table> {
    let p = root.join("themes").join(format!("{name}.toml"));
    let s = fs::read_to_string(&p).with_context(|| format!("theme file {}", p.display()))?;
    let mut ctx: toml::Table =
        toml::from_str(&s).with_context(|| format!("parse {}", p.display()))?;
    // Environment bindings available to every template.
    if !ctx.contains_key("home") {
        if let Ok(h) = env::var("HOME") {
            ctx.insert("home".into(), toml::Value::String(h));
        }
    }
    Ok(ctx)
}

fn apply(root: &Path, target: &Path, ctx: &toml::Table, theme: &str, dry: bool) -> Result<()> {
    let source = root.join("configs");
    if !source.is_dir() {
        return Err(anyhow!("source {} is not a directory", source.display()));
    }
    let env = Environment::new();

    println!("theme:  {theme}");
    println!("source: {}", source.display());
    println!("target: {}", target.display());
    if dry {
        println!("(dry run — no files will be written)");
    }
    println!();

    let mut changed = 0usize;
    let mut unchanged = 0usize;
    for entry in WalkDir::new(&source).min_depth(1) {
        let entry = entry.context("walk")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(&source).context("strip_prefix")?;
        let dest = target.join(rel);
        let raw =
            fs::read(entry.path()).with_context(|| format!("read {}", entry.path().display()))?;

        // Binary files (e.g. asset PNGs) are copied byte-for-byte. Text files
        // are templated through minijinja.
        let rendered: Vec<u8> = match std::str::from_utf8(&raw) {
            Ok(text) => env
                .render_str(text, ctx)
                .with_context(|| format!("render {}", rel.display()))?
                .into_bytes(),
            Err(_) => raw,
        };

        let current = fs::read(&dest).ok();
        let needs_write = current.as_deref() != Some(rendered.as_slice());

        if !needs_write {
            println!("  = {}", rel.display());
            unchanged += 1;
            continue;
        }

        if dry {
            println!("  ~ {}", rel.display());
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            fs::write(&dest, &rendered).with_context(|| format!("write {}", dest.display()))?;
            println!("  + {}", rel.display());
        }
        changed += 1;
    }

    println!();
    println!("binary:");
    install(dry)?;

    println!();
    println!("bridges:");
    ensure_bridges(dry)?;

    println!();
    println!("knowledge:");
    ensure_qdrant(dry)?;
    ensure_pdftotext(dry)?;
    ensure_cuda_runtime(dry)?;

    println!();
    let verb = if dry { "would change" } else { "changed" };
    println!("{verb} {changed}, unchanged {unchanged}");
    Ok(())
}

const QDRANT_VERSION: &str = "1.12.4";

/// Download the qdrant binary into `~/.local/bin/qdrant` if missing or
/// out-of-date. Mirrors `ensure_bridges` for the ACP-bridge npm package.
fn ensure_qdrant(dry: bool) -> Result<()> {
    use std::process::Command;

    let home = env::var("HOME").context("HOME not set")?;
    let dest_dir = Path::new(&home).join(".local/bin");
    let dest = dest_dir.join("qdrant");

    let installed_version = if dest.exists() {
        Command::new(&dest)
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    if let Some(v) = &installed_version {
        if v.contains(QDRANT_VERSION) {
            println!("  = qdrant {v}");
            return Ok(());
        }
    }

    let url = format!(
        "https://github.com/qdrant/qdrant/releases/download/v{ver}/qdrant-x86_64-unknown-linux-gnu.tar.gz",
        ver = QDRANT_VERSION
    );
    if dry {
        let from = installed_version.as_deref().unwrap_or("(missing)");
        println!(
            "  ~ qdrant {from} → {QDRANT_VERSION} ({dest})",
            dest = dest.display()
        );
        return Ok(());
    }

    fs::create_dir_all(&dest_dir)?;
    let tmp_tar = dest_dir.join("qdrant.tar.gz");

    // Use system curl so we don't pay the cost of pulling reqwest into the
    // installer path. Mirrors how `ensure_bridges` shells to npm.
    let st = Command::new("curl")
        .args(["-fL", "--retry", "3", "-o"])
        .arg(&tmp_tar)
        .arg(&url)
        .status()
        .with_context(|| format!("curl {url}"))?;
    if !st.success() {
        return Err(anyhow!("curl: failed to download {url}"));
    }
    let st = Command::new("tar")
        .args(["-xzf"])
        .arg(&tmp_tar)
        .args(["-C"])
        .arg(&dest_dir)
        .status()
        .context("tar -xzf qdrant")?;
    if !st.success() {
        return Err(anyhow!("tar: extract failed"));
    }
    let _ = fs::remove_file(&tmp_tar);

    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(&dest)?.permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&dest, perm)?;
    let _ = Command::new("restorecon").arg(&dest).status();

    println!("  + {} (v{QDRANT_VERSION})", dest.display());
    Ok(())
}

/// Probe `pdftotext`. We don't auto-install (needs sudo) — just print a
/// hint so the user knows PDFs will be skipped at index time.
fn ensure_pdftotext(_dry: bool) -> Result<()> {
    if which("pdftotext") {
        println!("  = pdftotext");
    } else {
        eprintln!(
            "  ! pdftotext missing — install with: sudo dnf install -y poppler-utils\n    \
             (PDFs will be skipped at index time until installed)"
        );
    }
    Ok(())
}

/// Probe the CUDA runtime libraries fastembed-rs/ort need. Doesn't fail
/// — embeddings fall back to CPU. Prints actionable install hints when
/// libraries are missing.
fn ensure_cuda_runtime(_dry: bool) -> Result<()> {
    use std::process::Command;

    let smi = Command::new("nvidia-smi")
        .args(["--query-gpu=name,driver_version", "--format=csv,noheader"])
        .output();
    let gpu_line = match smi {
        Ok(o) if o.status.success() => Some(
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
        ),
        _ => None,
    };

    let probe = |name: &str| -> bool {
        // Probe via `ldconfig -p` (covers system-wide). Fallback to
        // /usr/lib64 + /usr/local/cuda/lib64 file existence.
        let ldconfig = Command::new("ldconfig").arg("-p").output();
        if let Ok(o) = ldconfig {
            if String::from_utf8_lossy(&o.stdout).contains(name) {
                return true;
            }
        }
        for d in ["/usr/lib64", "/usr/lib", "/usr/local/cuda/lib64"] {
            if Path::new(d).join(name).exists() {
                return true;
            }
        }
        false
    };

    let cuda_driver = probe("libcuda.so.1");
    let cuda_runtime = probe("libcudart.so") || probe("libcudart.so.12");
    let cudnn = probe("libcudnn.so") || probe("libcudnn.so.9") || probe("libcudnn.so.8");

    let all_present = cuda_driver && cuda_runtime && cudnn;

    match (gpu_line, all_present) {
        (Some(gpu), true) => println!("  = cuda runtime ok ({gpu})"),
        (Some(gpu), false) => {
            eprintln!("  ! cuda runtime incomplete ({gpu})");
            eprintln!(
                "    driver libcuda={cuda_driver}  runtime libcudart={cuda_runtime}  cudnn={cudnn}"
            );
            eprintln!(
                "    embeddings will fall back to CPU. install with:\n    \
                 sudo dnf install -y akmod-nvidia xorg-x11-drv-nvidia-cuda libcudnn9 libcudnn9-devel"
            );
        }
        (None, _) => {
            eprintln!("  ! nvidia-smi not found — no GPU detected; embeddings will run on CPU");
        }
    }
    Ok(())
}

/// Ensure ACP bridge npm packages declared by sy are present at the latest
/// published version. Today this is just `@zed-industries/claude-code-acp`
/// (claude itself doesn't speak ACP natively; this Node bridge translates).
///
/// Installs into `$HOME/.local` (user-local prefix) so we don't require root
/// and so the binary ends up on the same PATH that sy itself uses
/// (`$HOME/.local/bin`). Idempotent.
fn ensure_bridges(dry: bool) -> Result<()> {
    use std::process::Command;

    if !which("npm") {
        eprintln!("  ! npm missing — skipping ACP bridge install");
        return Ok(());
    }

    let prefix = npm_prefix()?;
    // Uninstall the legacy @zed-industries package if it lingers from earlier
    // sy versions; it was renamed upstream.
    if npm_installed_version(&prefix, "@zed-industries/claude-code-acp").is_some() && !dry {
        let _ = std::process::Command::new("npm")
            .args(["uninstall", "--prefix"])
            .arg(&prefix)
            .args(["-g", "@zed-industries/claude-code-acp"])
            .status();
    }
    let pkg = "@agentclientprotocol/claude-agent-acp";
    let installed = npm_installed_version(&prefix, pkg);
    let latest = npm_latest_version(pkg);

    let current = installed.as_deref().unwrap_or("(missing)");
    let target = latest.as_deref().unwrap_or("?");

    // npm 11 sometimes leaves bin targets at mode 644; heal whatever is on
    // disk first so a transient npm failure below still leaves a working
    // launcher.
    if installed.is_some() {
        ensure_bin_executable(&prefix, pkg);
    }

    if installed.is_some() && installed.as_deref() == latest.as_deref() {
        println!("  = npm:{pkg}@{current}");
        return Ok(());
    }
    if dry {
        println!(
            "  ~ npm install --prefix {} -g {pkg}  ({current} → {target})",
            prefix.display()
        );
        return Ok(());
    }
    let st = Command::new("npm")
        .args(["install", "--prefix"])
        .arg(&prefix)
        .args(["-g", pkg])
        .status()?;
    if !st.success() {
        return Err(anyhow!("npm install -g {pkg} failed"));
    }
    ensure_bin_executable(&prefix, pkg);
    println!("  + npm:{pkg}@{target}");
    Ok(())
}

/// Make sure every `bin` entry in the installed package's manifest is
/// executable. Some packages (notably `@agentclientprotocol/claude-agent-acp`)
/// ship the dist file at mode 644 and rely on npm to flip the +x bit; npm 11
/// no longer does this reliably for ESM entrypoints, leaving the symlink in
/// `~/.local/bin/` pointing at a non-executable file (EACCES on spawn).
fn ensure_bin_executable(prefix: &Path, pkg: &str) {
    use std::os::unix::fs::PermissionsExt;

    let pkg_dir = prefix.join("lib/node_modules").join(pkg);
    let manifest = match fs::read_to_string(pkg_dir.join("package.json")) {
        Ok(s) => s,
        Err(_) => return,
    };
    let v: serde_json::Value = match serde_json::from_str(&manifest) {
        Ok(v) => v,
        Err(_) => return,
    };
    let entries: Vec<String> = match v.get("bin") {
        Some(serde_json::Value::Object(map)) => map
            .values()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        _ => return,
    };
    for rel in entries {
        let target = pkg_dir.join(&rel);
        let Ok(meta) = fs::metadata(&target) else {
            continue;
        };
        let mut perm = meta.permissions();
        let mode = perm.mode();
        let new = mode | 0o111;
        if new != mode {
            perm.set_mode(new);
            if fs::set_permissions(&target, perm).is_ok() {
                println!("  + chmod +x {}", target.display());
            }
        }
    }
}

fn npm_prefix() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local"))
}

fn npm_installed_version(prefix: &Path, pkg: &str) -> Option<String> {
    use std::process::Command;
    let out = Command::new("npm")
        .args(["ls", "--prefix"])
        .arg(prefix)
        .args(["-g", "--depth=0", "--json", pkg])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("dependencies")
        .and_then(|d| d.get(pkg))
        .and_then(|p| p.get("version"))
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

fn npm_latest_version(pkg: &str) -> Option<String> {
    use std::process::Command;
    let out = Command::new("npm")
        .args(["view", pkg, "version"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn tg_theme() -> Result<()> {
    use std::process::Command;

    let home = env::var("HOME").context("HOME not set")?;
    let palette = Path::new(&home).join(".config/telegram-desktop/palette.tdesktop-palette");
    if !palette.exists() {
        return Err(anyhow!(
            "{} not found — run `sy apply` first",
            palette.display()
        ));
    }

    let flatpak = Command::new("flatpak")
        .args(["info", "org.telegram.desktop"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if flatpak {
        // Grant persistent read access so Telegram's file picker can see
        // ~/.config/telegram-desktop/ without going through the portal each
        // time. Idempotent.
        let _ = Command::new("flatpak")
            .args([
                "override",
                "--user",
                "--filesystem=xdg-config/telegram-desktop:ro",
                "org.telegram.desktop",
            ])
            .status();
    }

    eprintln!("palette: {}", palette.display());
    eprintln!();
    eprintln!("To apply (Telegram doesn't auto-import palette files):");
    eprintln!();
    eprintln!("  1. Open Telegram");
    eprintln!("  2. Settings → Chat Settings → Color theme");
    eprintln!("  3. Click the \"…\" menu on any theme → \"Create new theme\"");
    eprintln!("  4. In the editor: \"…\" menu → \"Load from file\"");
    eprintln!("  5. Browse to:");
    eprintln!("       {}", palette.display());
    eprintln!();
    eprintln!("Shortcut: drag palette.tdesktop-palette onto the Telegram window.");
    eprintln!();

    if flatpak {
        let _ = Command::new("flatpak")
            .args(["run", "org.telegram.desktop"])
            .spawn();
    } else if which("telegram-desktop") {
        let _ = Command::new("telegram-desktop").spawn();
    } else if which("Telegram") {
        let _ = Command::new("Telegram").spawn();
    }
    Ok(())
}

pub fn which(name: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn install(dry: bool) -> Result<()> {
    use std::process::Command;

    let src = env::current_exe().context("locate running binary")?;
    let home = env::var("HOME").context("HOME not set")?;
    let dest_dir = Path::new(&home).join(".local/bin");
    let dest = dest_dir.join("sy");

    // No-op when the running binary is already the installed one.
    if let (Ok(s), Ok(d)) = (src.canonicalize(), dest.canonicalize()) {
        if s == d {
            println!("  = {} (already installed)", dest.display());
            return Ok(());
        }
    }

    let bytes = fs::read(&src).with_context(|| format!("read {}", src.display()))?;

    if dry {
        println!("  ~ {} ({} bytes)", dest.display(), bytes.len());
        return Ok(());
    }

    fs::create_dir_all(&dest_dir).with_context(|| format!("mkdir {}", dest_dir.display()))?;

    // Remove any existing file/symlink first; waybar can't follow a symlink
    // whose target has the wrong SELinux label, so we always install a real
    // copy.
    if dest.symlink_metadata().is_ok() {
        fs::remove_file(&dest).with_context(|| format!("rm {}", dest.display()))?;
    }

    fs::write(&dest, &bytes).with_context(|| format!("write {}", dest.display()))?;

    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(&dest)?.permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&dest, perm)?;

    // Restore the default ~/.local/bin label (home_bin_t on Fedora) so
    // waybar can execute it from its systemd scope.
    let _ = Command::new("restorecon").arg(&dest).status();

    println!("  + {} ({} bytes)", dest.display(), bytes.len());
    Ok(())
}

fn list_themes(root: &Path) -> Result<()> {
    let dir = root.join("themes");
    if !dir.is_dir() {
        return Ok(());
    }
    let mut names: Vec<String> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|s| s.to_str()) == Some("toml"))
                .then(|| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                })
                .flatten()
        })
        .collect();
    names.sort();
    for n in names {
        println!("{n}");
    }
    Ok(())
}
