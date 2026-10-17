//! Statusbar row composing the SPEC §3.3 item 4 contract — path
//! crumbs (left) + mode chip + selection chip + knowledge chip + ops
//! chip (right). Roadmap Step 25.
//!
//! The whole row is wrapped at the top of [`crate::file::view::root`]
//! by the `view()` callback in `crate::file::app`, sitting above the
//! pane composition. The commandbar lives at the bottom of the same
//! column.
//!
//! The pure helper [`crumb_tokens`] is the journey-J7 introspection
//! hook — iced 0.14's `Element` has no public introspection API so the
//! Step 25 unit test asserts against the token list directly (same
//! "escape hatch" pattern Step 24's `root_descriptor` uses).

use std::path::Path;

use iced::widget::{container, row, text};
use iced::{Element, Length};

use crate::file::app::Message;
use crate::file::ipc::OpRow;
use crate::file::search::knowledge::KnowledgeStatus;
use crate::file::state::{Operation, State};
use crate::file::widgets::chip::{knowledge_chip, mode_chip, selection_chip};
use crate::file::widgets::crumb::crumb;
use crate::file::widgets::progress_row::progress_row;

/// Knowledge-chip label when the daemon is reachable but no `:k`
/// query has fired yet. Pinned as a `pub const` so the Step 30
/// backend can swap the literal in without the view layer rewriting.
pub const KNOWLEDGE_CHIP_IDLE: &str = "knowledge: idle";

/// Knowledge-chip label after a successful query. The reducer plants
/// the matched count on `state.knowledge.last_hits.len()`; the chip
/// surfaces it inline.
pub const KNOWLEDGE_CHIP_PREFIX: &str = "knowledge:";

/// Knowledge-chip label when `sy-knowledge.service` is unreachable.
/// SPEC §6 risk row 3 — the operator sees the chip dim-grey instead
/// of a "search returned 0" silence.
pub const KNOWLEDGE_CHIP_UNREACHABLE: &str = "knowledge: unreachable";

/// Knowledge-chip label when the 250 ms budget elapsed.
pub const KNOWLEDGE_CHIP_TIMEOUT: &str = "knowledge: timeout";

/// Return the label the chip paints given the current
/// [`KnowledgeStatus`] + (optional) last-hit count. Pure helper so
/// the Step 30 chip-status test can pin the label table without
/// driving an iced render.
pub fn knowledge_chip_label(status: KnowledgeStatus, hits: usize) -> String {
    match status {
        KnowledgeStatus::Reachable if hits == 0 => KNOWLEDGE_CHIP_IDLE.to_owned(),
        KnowledgeStatus::Reachable => format!("{KNOWLEDGE_CHIP_PREFIX} {hits} hits"),
        KnowledgeStatus::Unreachable => KNOWLEDGE_CHIP_UNREACHABLE.to_owned(),
        KnowledgeStatus::Timeout => KNOWLEDGE_CHIP_TIMEOUT.to_owned(),
    }
}

/// Ops-chip label when no operation is queued or in-flight.
pub const OPS_CHIP_IDLE: &str = "ops: 0";

/// Render the statusbar row. Composition (left → right):
///
/// 1. Path crumbs derived from `state.panes.current.cwd`.
/// 2. Spacer (fills remaining width).
/// 3. Mode chip ([`crate::file::widgets::chip::mode_chip`]).
/// 4. Selection chip.
/// 5. Knowledge chip — idle today, populated by Step 30.
/// 6. Ops chip — `{queue_len} ops`.
pub fn statusbar(state: &State) -> Element<'_, Message> {
    let crumbs = crumb(&state.panes.current.cwd);
    let knowledge = knowledge_chip(state.knowledge.status, state.knowledge.last_hits.len());
    let ops_label = if state.ops.is_empty() {
        OPS_CHIP_IDLE.to_owned()
    } else {
        format!(
            "ops: {} ({})",
            state.ops.len(),
            ops_verb_summary(&state.ops)
        )
    };
    let ops = container(text(ops_label)).padding(4);
    container(
        row![
            crumbs,
            container(text("")).width(Length::Fill),
            mode_chip(state.mode),
            selection_chip(state.selection.len()),
            knowledge,
            ops,
        ]
        .spacing(8),
    )
    .width(Length::Fill)
    .padding(4)
    .into()
}

/// Roll the queued ops into a single short verb summary
/// (`"2 copy + 1 trash"`) for the ops chip body. Tested below so the
/// label table stays observable without driving an iced render.
pub fn ops_verb_summary(ops: &[Operation]) -> String {
    let mut copy = 0_u32;
    let mut moves = 0_u32;
    let mut trash = 0_u32;
    let mut other = 0_u32;
    for op in ops {
        match op {
            Operation::Copy { .. } => copy += 1,
            Operation::Move { .. } => moves += 1,
            Operation::Trash { .. } => trash += 1,
            _ => other += 1,
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if copy > 0 {
        parts.push(format!("{copy} copy"));
    }
    if moves > 0 {
        parts.push(format!("{moves} move"));
    }
    if trash > 0 {
        parts.push(format!("{trash} trash"));
    }
    if other > 0 {
        parts.push(format!("{other} other"));
    }
    parts.join(" + ")
}

/// Build the per-op progress drawer. The view returns an `Element`
/// when at least one op is queued in `state.ops`; otherwise an empty
/// shrink-container so the statusbar's height stays stable.
///
/// Each queued `Operation` is wrapped in a synthesised [`OpRow`]
/// (kind tag, `state = "queued"`, `done/total = 0`) and rendered with
/// [`progress_row`]; the wire shape matches what the daemon emits via
/// `file.ops_list` so a future patch that pipes daemon `OpRow`s
/// straight into `State` doesn't have to reshape this view.
pub fn ops_drawer(state: &State) -> Element<'_, Message> {
    if state.ops.is_empty() {
        return container(text("")).width(Length::Shrink).into();
    }
    let mut col = iced::widget::column![].spacing(2);
    for (idx, op) in state.ops.iter().enumerate() {
        let row = OpRow {
            op_id: idx as u64,
            kind: verb_for(op).to_owned(),
            state: "queued".to_owned(),
            done: 0,
            total: 0,
        };
        col = col.push(progress_row(row));
    }
    container(col).width(Length::Fill).padding(2).into()
}

/// Short verb tag for an [`Operation`]. Mirrors the daemon's `OpRow`
/// `kind` field so the GUI + IPC wire shape agree.
fn verb_for(op: &Operation) -> &'static str {
    match op {
        Operation::Copy { .. } => "copy",
        Operation::Move { .. } => "move",
        Operation::Trash { .. } => "trash",
        Operation::Restore { .. } => "restore",
        Operation::Mkdir { .. } => "mkdir",
        Operation::Rename { .. } => "rename",
    }
}

/// Pure helper: tokenise `cwd` into the breadcrumb segments the
/// statusbar paints. Substitutes `$HOME` with `~` so the operator
/// sees a tilde-prefixed path instead of `/home/<user>/…` (the
/// journey-J2 "I recognise my home dir" affordance).
///
/// Returns the segments left → right, e.g. `cwd=/home/dmitriy/sources/sy`
/// + `home=/home/dmitriy` → `["~", "sources", "sy"]`.
pub fn crumb_tokens(cwd: &Path, home: &Path) -> Vec<String> {
    // Strip a `home` prefix → prepend `~` segment, else fall through
    // to plain path components.
    let (lead, rest) = match cwd.strip_prefix(home) {
        Ok(suffix) => (Some("~"), suffix),
        Err(_) => (None, cwd),
    };
    let mut out: Vec<String> = Vec::new();
    if let Some(tilde) = lead {
        out.push(tilde.to_owned());
    }
    for comp in rest.components() {
        // Skip the root component when there's no `~` lead — `cwd=/`
        // produces an empty token list per SPEC §3.3 row 4.
        if matches!(
            comp,
            std::path::Component::RootDir | std::path::Component::Prefix(_)
        ) {
            continue;
        }
        out.push(comp.as_os_str().to_string_lossy().into_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Roadmap Step 25 pin: a `cwd` under `$HOME` collapses the home
    /// prefix to `~`. The journey-J2 brief assumes the statusbar
    /// reads `~/sources/sy`, not `/home/dmitriy/sources/sy`.
    #[test]
    fn crumb_renders_relative_to_home() {
        let cwd = PathBuf::from("/home/dmitriy/sources/sy");
        let home = PathBuf::from("/home/dmitriy");
        let tokens = crumb_tokens(&cwd, &home);
        assert_eq!(
            tokens,
            vec!["~".to_string(), "sources".to_string(), "sy".to_string()],
            "cwd under HOME must collapse to ~/sources/sy tokens"
        );
    }

    /// A cwd NOT under `$HOME` falls through to plain path components
    /// — `/etc/sy` becomes `["etc", "sy"]`. Defends against a regex
    /// rewrite that always prepends `~`.
    #[test]
    fn crumb_outside_home_renders_absolute_components() {
        let cwd = PathBuf::from("/etc/sy");
        let home = PathBuf::from("/home/dmitriy");
        let tokens = crumb_tokens(&cwd, &home);
        assert_eq!(tokens, vec!["etc".to_string(), "sy".to_string()]);
    }

    /// Root `/` produces an empty token list — the statusbar then
    /// paints just the chips on the right.
    #[test]
    fn crumb_root_is_empty_tokens() {
        let tokens = crumb_tokens(&PathBuf::from("/"), &PathBuf::from("/home/agent"));
        assert!(
            tokens.is_empty(),
            "root cwd must produce no crumb tokens, got {tokens:?}"
        );
    }

    /// Step 28 DoD: the ops chip summarises the queued verbs as
    /// `"N copy + M trash"` so the journey-J6 ops chip reads true to
    /// the queue body at a glance.
    #[test]
    fn ops_verb_summary_groups_by_verb() {
        use crate::file::state::{ConflictPolicy, Operation};
        let ops = vec![
            Operation::Copy {
                srcs: vec![],
                dst: PathBuf::from("/"),
                conflict: ConflictPolicy::Skip,
            },
            Operation::Copy {
                srcs: vec![],
                dst: PathBuf::from("/"),
                conflict: ConflictPolicy::Skip,
            },
            Operation::Trash { srcs: vec![] },
        ];
        assert_eq!(ops_verb_summary(&ops), "2 copy + 1 trash");
    }
}
