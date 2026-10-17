use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "sy", version, about = "sy authentication controls")]
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
    /// Phone key status, PAM setup, and Waybar state.
    Syauth {
        /// accept | reject | status | install-pam | uninstall-pam | doctor
        action: Option<String>,
        #[arg(long, conflicts_with = "action")]
        waybar: bool,
        #[arg(long)]
        service: Option<String>,
        #[arg(long)]
        control: Option<String>,
        #[arg(long)]
        yes: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let _ = (cli.root, cli.target);
    match cli.command {
        Command::Syauth {
            action,
            waybar,
            service,
            control,
            yes,
        } => sy_auth::syauth::run_cli(
            action.as_deref(),
            waybar,
            service.as_deref(),
            control.as_deref(),
            yes,
        ),
    }
}
