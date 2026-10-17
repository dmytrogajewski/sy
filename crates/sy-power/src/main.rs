use clap::Parser;

#[derive(Parser)]
#[command(name = "sy power", about = "Lock or end the desktop session")]
struct Cli {
    action: Option<String>,
    #[arg(long, env = "SY_POWER_DRY_RUN")]
    dry_run: bool,
    #[arg(long, env = "SY_POWER_YES")]
    yes: bool,
    #[arg(long, env = "SY_POWER_JSON")]
    json: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    sy_power::run(cli.action.as_deref(), cli.dry_run, cli.yes, cli.json)
}
