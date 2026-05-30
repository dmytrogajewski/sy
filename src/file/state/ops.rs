//! `Operation` queue + `OpEvent` stream. Step 14 of the
//! [`sy-file-manager` roadmap][roadmap] models the file-op verbs SPEC
//! §3.3 row 5 ("copy, move, rename, trash, restore, mkdir") and the
//! journey-J6 progress beat. Step 16+ binds these to real fs work; the
//! types ship today as pure data so the state machine + IPC contract
//! (Step 20) can compile against them.
//!
//! `OpEvent` carries a `kind` JSON discriminator
//! (`#[serde(tag = "kind", rename_all = "snake_case")]`) so the wire
//! shape stays stable across the host ↔ agent boundary. The roadmap DoD
//! pins this exact serde shape — drift would silently re-route every
//! later step's progress UI.
//!
//! [roadmap]: ../../../../specs/roadmaps/sy-file-manager/ROADMAP.md

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::selection::EntryId;

/// On-conflict policy for [`Operation::Copy`] / [`Operation::Move`].
/// Mirrors Cosmic Files' three-choice prompt (SPEC §3.2 row 4) so the
/// journey-J6 dst-collision branch maps to a typed variant instead of a
/// stringly-typed flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Leave the destination untouched, skip the source.
    Skip,
    /// Overwrite the destination in place.
    Overwrite,
    /// Auto-suffix the destination (`name.1`, `name.2`, …).
    Rename,
}

/// Queued or in-flight file operation. The `srcs`/`dst` shape lets us
/// batch a multi-select (journey-J5 ↔ J6 hand-off) into one task; the
/// `conflict` field is the policy the user picked at queue-time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum Operation {
    /// Copy `srcs` into `dst` directory under `conflict`.
    Copy {
        srcs: Vec<PathBuf>,
        dst: PathBuf,
        conflict: ConflictPolicy,
    },
    /// Move `srcs` into `dst` directory under `conflict`. Same-fs path
    /// uses `renameat2`; cross-fs falls back to copy+unlink.
    Move {
        srcs: Vec<PathBuf>,
        dst: PathBuf,
        conflict: ConflictPolicy,
    },
    /// Send each of `srcs` to the freedesktop trash. Step 18's
    /// `fs::trash` binds this to the `trash` crate.
    Trash { srcs: Vec<PathBuf> },
    /// Restore the named trashed entries by id. Pairs with
    /// [`Operation::Trash`]; the ids are the trash-info keys the trash
    /// crate hands back.
    Restore { ids: Vec<EntryId> },
    /// Create `parent/name`. The two-field shape (instead of one full
    /// `PathBuf`) keeps the UI prompt that drives this op aligned with
    /// what the user typed.
    Mkdir { parent: PathBuf, name: String },
    /// Rename `src` (a full path) to `new_name` in the same parent dir.
    Rename { src: PathBuf, new_name: String },
}

/// Stream of progress events emitted by an in-flight [`Operation`].
/// SPEC §3.3 row 5 pins this exact variant set; the journey-J6 progress
/// pill consumes them. Step 20's IPC layer round-trips them as JSON, so
/// the `kind` discriminator is part of the public contract — see the
/// `op_event_serde_roundtrip` test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpEvent {
    /// The op left the queue and started executing.
    Started { op_id: u64 },
    /// Periodic progress sample. The journey-J6 acceptance criterion
    /// (≥10 Hz, every 100 ms or 4 MiB whichever first) is enforced at
    /// the emission site in Step 16; here we only pin the wire shape.
    Progress {
        op_id: u64,
        done: u64,
        total: u64,
        throughput_bps: u64,
    },
    /// User pressed pause; the executor halted at a safe checkpoint.
    Paused { op_id: u64 },
    /// User pressed resume.
    Resumed { op_id: u64 },
    /// User cancelled; partial dst was rolled back.
    Cancelled { op_id: u64 },
    /// Op finished cleanly.
    Completed { op_id: u64 },
    /// Op failed; `code` is the POSIX errno (or a sy-specific exit
    /// code) and `msg` is the operator-actionable explanation.
    Failed { op_id: u64, code: i32, msg: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 14 DoD bullet: every `OpEvent` variant round-trips through
    /// JSON and carries the snake_case `kind` discriminator the wire
    /// contract pins. Drift here would silently re-route Step 20's IPC.
    #[test]
    fn op_event_serde_roundtrip() {
        let cases: Vec<(OpEvent, &str)> = vec![
            (OpEvent::Started { op_id: 1 }, "started"),
            (
                OpEvent::Progress {
                    op_id: 1,
                    done: 256,
                    total: 1024,
                    throughput_bps: 4096,
                },
                "progress",
            ),
            (OpEvent::Paused { op_id: 1 }, "paused"),
            (OpEvent::Resumed { op_id: 1 }, "resumed"),
            (OpEvent::Cancelled { op_id: 1 }, "cancelled"),
            (OpEvent::Completed { op_id: 1 }, "completed"),
            (
                OpEvent::Failed {
                    op_id: 1,
                    code: 28,
                    msg: "ENOSPC".to_owned(),
                },
                "failed",
            ),
        ];
        for (ev, want_kind) in cases {
            let v = serde_json::to_value(&ev).expect("to_value");
            assert_eq!(
                v.get("kind").and_then(|k| k.as_str()),
                Some(want_kind),
                "OpEvent must serialise with kind = {want_kind:?}, got {v}",
            );
            let back: OpEvent = serde_json::from_value(v).expect("from_value");
            assert_eq!(back, ev, "OpEvent must round-trip through JSON value");
        }
    }
}
