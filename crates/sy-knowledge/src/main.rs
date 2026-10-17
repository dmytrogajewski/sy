use anyhow::Result;
use clap::{Parser, Subcommand};
use sy_knowledge::{auto, knowledge};

#[derive(Parser)]
#[command(name = "sy", version, about = "sy semantic knowledge plane")]
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
    Knowledge {
        #[command(subcommand)]
        sub: knowledge::KnowledgeCmd,
    },
    Auto {
        #[command(subcommand)]
        sub: auto::AutoCmd,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(root) = cli.root {
        // SAFETY: startup is single threaded and this runs before any daemon or CLI worker starts.
        unsafe { std::env::set_var("SY_ROOT", root) };
    }
    let _ = cli.target;
    sy_knowledge::aiplane::reexec::maybe_reexec_with_amd_env();
    let owns_subscriber = matches!(
        &cli.command,
        Command::Knowledge {
            sub: knowledge::KnowledgeCmd::Daemon
        }
    );
    let _obs_guard = sy_core::obs::init_cli_unless(owns_subscriber)?;
    let result = sy_core::obs::with_trace_id(sy_core::TraceId::new(), None, || match cli.command {
        Command::Knowledge { sub } => knowledge::dispatch(sub),
        Command::Auto { sub } => auto::dispatch(sub),
    });
    if let Err(error) = &result
        && let Some(domain) = error.downcast_ref::<knowledge::KnowledgeError>()
    {
        eprintln!("error: {}", domain.msg);
        std::process::exit(domain.code);
    }
    result
}
