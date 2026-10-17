use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use sy_agent::agt;

#[derive(Parser)]
#[command(name = "sy", version, about = "sy agent runner and policy controls")]
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
    Agt {
        #[command(subcommand)]
        sub: agt::AgtCmd,
    },
    Policy {
        #[command(subcommand)]
        cmd: agt::policy::cli::PolicyCmd,
    },
    Approve {
        token: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        token_from_stdin: bool,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(root) = cli.root {
        // SAFETY: this CLI sets its root override before starting worker threads; no concurrent environment access exists.
        unsafe { std::env::set_var("SY_ROOT", root) };
    }
    let _ = cli.target;
    let _obs_guard = sy_core::obs::init_cli_unless(matches!(
        &cli.command,
        Command::Agt {
            sub: agt::AgtCmd::Daemon
        }
    ))?;
    let result = sy_core::obs::with_trace_id(sy_core::TraceId::new(), None, || match cli.command {
        Command::Agt { sub } => agt::dispatch(sub),
        Command::Policy { cmd } => agt::policy::cli::dispatch(cmd),
        Command::Approve {
            token,
            yes,
            token_from_stdin,
            json,
        } => agt::policy::cli::approve(token, yes, token_from_stdin, json),
    });
    if let Err(error) = &result
        && let Some(agent_error) = error.downcast_ref::<agt::AgtError>()
    {
        eprintln!("error: {}", agent_error.msg);
        std::process::exit(agent_error.code);
    }
    result
}
