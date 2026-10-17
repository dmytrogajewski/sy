use clap::{Parser, Subcommand};
use sy_knowledge::{auto::AutoCmd, knowledge::KnowledgeCmd};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Knowledge {
        #[command(subcommand)]
        sub: KnowledgeCmd,
    },
    Auto {
        #[command(subcommand)]
        sub: AutoCmd,
    },
}

#[test]
fn knowledge_and_auto_commands_remain_available() {
    assert!(matches!(
        Cli::try_parse_from(["sy", "knowledge", "daemon"])
            .unwrap()
            .cmd,
        Cmd::Knowledge {
            sub: KnowledgeCmd::Daemon
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["sy", "auto", "list-detectors"])
            .unwrap()
            .cmd,
        Cmd::Auto {
            sub: AutoCmd::ListDetectors { json: false }
        }
    ));
}
