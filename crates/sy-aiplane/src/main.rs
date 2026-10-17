use anyhow::Result;
use clap::{Parser, Subcommand};
use sy_aiplane::aiplane;

#[derive(Parser)]
#[command(name = "sy", version, about = "sy AI workloads and NPU tools")]
struct Cli {
    #[arg(long, global = true, env = "SY_ROOT")]
    root: Option<std::path::PathBuf>,
    #[arg(long, global = true)]
    target: Option<std::path::PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Aiplane {
        #[command(subcommand)]
        sub: aiplane::cli::AiplaneCmd,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(root) = cli.root {
        // SAFETY: this CLI sets its root override before starting worker threads; no concurrent environment access exists.
        unsafe { std::env::set_var("SY_ROOT", root) };
    }
    let _ = cli.target;
    aiplane::reexec::maybe_reexec_with_amd_env();
    let owns_subscriber = matches!(
        &cli.command,
        Command::Aiplane {
            sub: aiplane::cli::AiplaneCmd::Worker { .. } | aiplane::cli::AiplaneCmd::Daemon
        }
    );
    let _obs_guard = sy_core::obs::init_cli_unless(owns_subscriber)?;
    sy_core::obs::with_trace_id(sy_core::TraceId::new(), None, || match cli.command {
        Command::Aiplane { sub } => aiplane::cli::dispatch(sub),
    })
}
