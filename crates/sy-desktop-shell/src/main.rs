use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use sy_desktop_shell::{
    bat, bright, bt, cal, disk, gpu, net, notif, npu, popup, profile, silent, sound, stack, vol,
    wallpaper, wifi, wwan,
};

#[derive(Parser)]
#[command(name = "sy", version, about = "sy desktop shell")]
struct Cli {
    #[arg(long, global = true, env = "SY_ROOT")]
    root: Option<PathBuf>,
    #[arg(long, global = true)]
    target: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Popup {
        key: String,
    },
    Cal,
    Wifi,
    Net {
        #[arg(long)]
        waybar: bool,
    },
    Wallpaper {
        path: Option<PathBuf>,
        #[arg(long, conflicts_with_all = ["path", "default"])]
        start: bool,
        #[arg(long, conflicts_with_all = ["path", "start"])]
        default: bool,
    },
    Vol {
        action: Option<String>,
        #[arg(long, conflicts_with = "action")]
        waybar: bool,
    },
    Bright {
        action: Option<String>,
        #[arg(long, conflicts_with = "action")]
        waybar: bool,
    },
    Bat {
        #[arg(long)]
        waybar: bool,
    },
    #[command(visible_alias = "pwr")]
    Profile {
        #[arg(env = "SY_PROFILE_ACTION")]
        action: Option<String>,
        #[arg(long, conflicts_with_all = ["action", "json"])]
        waybar: bool,
        #[arg(long, conflicts_with = "waybar")]
        json: bool,
    },
    Gpu {
        #[arg(long)]
        waybar: bool,
    },
    Npu {
        #[arg(long)]
        waybar: bool,
    },
    Disk {
        #[arg(long)]
        waybar: bool,
        #[arg(long = "threshold-gib", value_name = "GIB")]
        threshold_gib: Option<u64>,
    },
    Notif {
        action: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    Bt {
        #[arg(long)]
        waybar: bool,
    },
    Wwan {
        action: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        yes: bool,
    },
    Silent {
        action: Option<String>,
        #[arg(long, conflicts_with = "action")]
        waybar: bool,
    },
    Snd {
        action: String,
    },
    Stack {
        #[command(subcommand)]
        sub: stack::StackCmd,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(root) = cli.root {
        // SAFETY: startup is single threaded and this runs before shell workers start.
        unsafe { std::env::set_var("SY_ROOT", root) };
    }
    let _ = cli.target;
    let _obs_guard = sy_core::obs::init_cli_unless(matches!(
        &cli.command,
        Cmd::Stack {
            sub: stack::StackCmd::Bar
        }
    ))?;
    let result = sy_core::obs::with_trace_id(sy_core::TraceId::new(), None, || match cli.command {
        Cmd::Popup { key } => popup::toggle(&key),
        Cmd::Cal => cal::run(),
        Cmd::Wifi => wifi::pick(),
        Cmd::Net { waybar } => net::run(waybar),
        Cmd::Wallpaper {
            path,
            start,
            default,
        } => wallpaper::run(path, start, default),
        Cmd::Vol { action, waybar } => vol::run(action.as_deref(), waybar),
        Cmd::Bright { action, waybar } => bright::run(action.as_deref(), waybar),
        Cmd::Bat { waybar } => bat::run(waybar),
        Cmd::Profile {
            action,
            waybar,
            json,
        } => profile::run(action.as_deref(), waybar, json),
        Cmd::Gpu { waybar } => gpu::run(waybar),
        Cmd::Npu { waybar } => npu::run(waybar),
        Cmd::Disk {
            waybar,
            threshold_gib,
        } => disk::run(waybar, threshold_gib),
        Cmd::Notif { action, rest } => notif::run(action.as_deref().unwrap_or("menu"), &rest),
        Cmd::Bt { waybar } => bt::run(waybar),
        Cmd::Wwan { action, json, yes } => wwan::run(action.as_deref(), json, yes),
        Cmd::Silent { action, waybar } => silent::run(action.as_deref(), waybar),
        Cmd::Snd { action } => match action.as_str() {
            "login" => sound::login(),
            "logout" => sound::logout(),
            "test" => sound::test(),
            other => Err(anyhow!("unknown snd action: {other} (login|logout|test)")),
        },
        Cmd::Stack { sub } => stack::dispatch(sub),
    });
    if let Err(error) = &result
        && let Some(stack) = error.downcast_ref::<stack::StackError>()
    {
        eprintln!("error: {}", stack.msg);
        std::process::exit(stack.code);
    }
    result
}
