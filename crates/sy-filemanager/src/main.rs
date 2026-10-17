use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use sy_filemanager::{file, plugin};

#[derive(Parser)]
#[command(name = "sy", version, about = "sy file manager and plugin host")]
struct Cli {
    #[arg(long, global = true, env = "SY_ROOT")]
    root: Option<PathBuf>,
    #[arg(long, global = true)]
    target: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    File {
        path: Option<PathBuf>,
        #[command(subcommand)]
        cmd: Option<file::cli::FileCmd>,
    },
    Plugin {
        #[command(subcommand)]
        cmd: plugin::cli::PluginCmd,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(root) = cli.root {
        // SAFETY: this CLI sets its root override before starting worker threads; no concurrent environment access exists.
        unsafe { std::env::set_var("SY_ROOT", root) };
    }
    let _ = cli.target;
    let _obs_guard = sy_core::obs::init_cli_unless(false)?;
    sy_core::obs::with_trace_id(sy_core::TraceId::new(), None, || match cli.command {
        Command::File { path, cmd } => file::cli::dispatch(path, cmd),
        Command::Plugin { cmd } => plugin::cli::dispatch(cmd),
    })
}
