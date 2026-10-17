use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use sy_monitor::mon;

#[derive(Parser)]
#[command(name = "sy", version, about = "sy system monitor")]
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
    Mon {
        #[command(subcommand)]
        cmd: Option<mon::cli::MonCmd>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(root) = cli.root {
        // SAFETY: this CLI sets its root override before starting worker threads; no concurrent environment access exists.
        unsafe { std::env::set_var("SY_ROOT", root) };
    }
    let _ = cli.target;
    let own_subscriber = match &cli.command {
        Command::Mon {
            cmd: Some(mon::cli::MonCmd::Collect(_)),
        } => true,
        #[cfg(feature = "gui-iced")]
        Command::Mon {
            cmd: Some(mon::cli::MonCmd::Open) | None,
        } => true,
        _ => false,
    };
    let _obs_guard = sy_core::obs::init_cli_unless(own_subscriber)?;
    let result = sy_core::obs::with_trace_id(sy_core::TraceId::new(), None, || match cli.command {
        Command::Mon { cmd } => {
            mon::cli::dispatch(cmd.unwrap_or_else(mon::cli::default_subcommand))
        }
    });
    if let Err(error) = &result
        && let Some(monitor) = error.downcast_ref::<mon::MonError>()
    {
        eprintln!("error: {}", monitor.msg);
        std::process::exit(monitor.code);
    }
    result
}
