//! Subscriber ownership for `main` — which subcommand turns into a
//! supervised long-lived plane and therefore installs its *own*
//! `tracing` stack (BUG-20260927-1500).
//!
//! `tracing` permits exactly one subscriber per process. `main` used to
//! install `Mode::Cli` unconditionally before dispatch, so every daemon
//! subcommand's later `obs::init(Mode::Daemon { .. })` hit
//! `try_init`-already-set and degraded to a graceful no-op. The visible
//! symptom: `journalctl -u sy-knowledge` carried only systemd-captured
//! stderr, and `$XDG_STATE_HOME/sy/logs/<plane>/` filled with 0-byte
//! daily files — the journald layer and the rolling JSONL appender
//! promised by SPEC §4.6 never existed in the running processes.
//!
//! `main` now asks the parsed command first and hands the slot over
//! (`sy_core::obs::init_cli_unless`). This lives under `supervision`
//! because "which subcommand is a supervised unit" is supervision's
//! domain, and `src/main.rs` is LOC-budgeted (`scripts/check_main_rs_loc.sh`).
//!
//! Drift guard: a future plane that forgets to register here still
//! starts, but `obs::init` prints a one-line notice to stderr naming the
//! unit and this file, so the lost sinks show up in the journal instead
//! of rotting silently.

use crate::agt::AgtCmd;
use crate::aiplane::cli::AiplaneCmd;
use crate::knowledge::KnowledgeCmd;
use crate::mon::cli::MonCmd;
use crate::stack::StackCmd;
use crate::Cmd;

impl Cmd {
    /// `true` when dispatching this command ends in a daemon entry
    /// point that calls `obs::init(Mode::Daemon { .. })`, so `main`
    /// must not claim the process-global subscriber slot first.
    ///
    /// The set mirrors the `Mode::Daemon` call sites and their units:
    /// `KnowledgeCmd::Daemon` (sy-knowledge), `AgtCmd::Daemon`
    /// (sy-agentd), `StackCmd::Bar` (sy-stack-bar),
    /// `AiplaneCmd::Worker` (sy-aiplane worker children),
    /// `MonCmd::Collect` (sy-mon-collect), and — with the GUI feature
    /// on — `MonCmd::Open` / bare `sy mon` (sy-mon-popup, which is also
    /// what `mon::cli::default_subcommand` resolves `None` to).
    pub(crate) fn installs_own_subscriber(&self) -> bool {
        match self {
            Self::Knowledge {
                sub: KnowledgeCmd::Daemon,
            }
            | Self::Agt {
                sub: AgtCmd::Daemon,
            }
            | Self::Stack { sub: StackCmd::Bar }
            | Self::Aiplane {
                sub: AiplaneCmd::Worker { .. },
            }
            | Self::Mon {
                cmd: Some(MonCmd::Collect(..)),
            } => true,
            #[cfg(feature = "gui-iced")]
            Self::Mon {
                cmd: Some(MonCmd::Open) | None,
            } => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::mon::cli::CollectOpts;

    fn collect() -> Cmd {
        Cmd::Mon {
            cmd: Some(MonCmd::Collect(CollectOpts {
                history_size: 60,
                tick_ms: 1000,
                bind: Some(PathBuf::from("/tmp/sy-test/mon.sock")),
                history_path: Some(PathBuf::from("/tmp/sy-test/mon.ring")),
            })),
        }
    }

    /// Every plane that installs `Mode::Daemon` must be declared here or
    /// its journald + rolling-JSONL sinks are silently lost.
    #[test]
    fn daemon_subcommands_declare_ownership() {
        let daemons = [
            Cmd::Knowledge {
                sub: KnowledgeCmd::Daemon,
            },
            Cmd::Agt {
                sub: AgtCmd::Daemon,
            },
            Cmd::Stack { sub: StackCmd::Bar },
            Cmd::Aiplane {
                sub: AiplaneCmd::Worker {
                    kind: "embed".into(),
                    socket: PathBuf::from("/tmp/sy-test/worker.sock"),
                },
            },
            collect(),
        ];
        for cmd in daemons {
            assert!(
                cmd.installs_own_subscriber(),
                "{} must defer the CLI subscriber",
                std::any::type_name_of_val(&cmd)
            );
        }
    }

    /// Short-lived CLI surfaces keep the stderr subscriber: deferring
    /// there would leave the process with no sink at all.
    #[test]
    fn cli_subcommands_keep_the_stderr_subscriber() {
        let cli_only = [
            Cmd::Themes,
            Cmd::Cal,
            Cmd::Wifi,
            Cmd::Knowledge {
                sub: KnowledgeCmd::List { json: true },
            },
        ];
        for cmd in cli_only {
            assert!(
                !cmd.installs_own_subscriber(),
                "{} is a one-shot CLI command",
                std::any::type_name_of_val(&cmd)
            );
        }
    }

    /// Bare `sy mon` resolves through `mon::cli::default_subcommand`:
    /// the popup (a plane) with the GUI feature, a snapshot read without.
    #[test]
    fn bare_mon_follows_the_default_subcommand() {
        let bare = Cmd::Mon { cmd: None };
        #[cfg(feature = "gui-iced")]
        assert_eq!(
            bare.installs_own_subscriber(),
            matches!(crate::mon::cli::default_subcommand(), MonCmd::Open),
        );
        #[cfg(not(feature = "gui-iced"))]
        assert!(!bare.installs_own_subscriber());
    }
}
