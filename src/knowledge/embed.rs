//! Thin façade over the aiplane embed worker. Exposes
//! `embed_one`, `embed_batch`, `current_backend`, and
//! `current_hardware` so `knowledge::daemon`, `knowledge::cli`, and
//! the MCP server route their embed traffic through one place. The
//! real ONNX session lives in `sy aiplane worker --kind embed`.
//!
//! There is exactly one path: callers route through the supervisor.
//! When the supervisor isn't running (e.g. a CLI process that hasn't
//! gone through the daemon), the helpers return an error rather than
//! silently spinning up an in-process ORT session — that mode caused
//! the multi-context-per-process collisions that the worker split was
//! designed to eliminate.

use anyhow::Result;

use crate::aiplane::error::AiplaneError;
use crate::aiplane::registry::{WorkloadInput, WorkloadKind, WorkloadOutput, WorkloadState};
use crate::aiplane::supervisor;

use super::{exit, KnowledgeError};

/// `"vitisai"`, `"cpu"`, `"loading"`, `"failed"`, `"not-prepared"`,
/// `"unavailable"`, or `"unloaded"` if the supervisor hasn't reported
/// yet. Surfaced to the status snapshot so the waybar tooltip +
/// `sy knowledge status` reflect the embed worker's true state.
pub fn current_backend() -> &'static str {
    let Some(sup) = supervisor::current() else {
        return "unloaded";
    };
    match sup.all_health().get(&WorkloadKind::Embed) {
        Some(Some(h)) => match &h.state {
            WorkloadState::Ready { backend } => match backend.as_str() {
                "vitisai" => "vitisai",
                "cpu" => "cpu",
                _ => "vitisai",
            },
            WorkloadState::Loading => "loading",
            WorkloadState::Failed { .. } => "failed",
            WorkloadState::NotPrepared => "not-prepared",
            WorkloadState::Unavailable => "unavailable",
        },
        _ => "unloaded",
    }
}

/// Human-readable label for the actual hardware doing inference,
/// e.g. `"AMD NPU on 9 HX 370"`, `"AMD Ryzen AI 9 HX 370 (CPU)"`.
/// Synthesised from the worker's reported backend + the host CPU
/// model.
pub fn current_hardware() -> String {
    match current_backend() {
        "vitisai" => format!(
            "AMD NPU on {}",
            crate::aiplane::workloads::detect_cpu_model()
                .strip_prefix("AMD Ryzen AI ")
                .unwrap_or("?")
        ),
        "cpu" => format!("{} (CPU)", crate::aiplane::workloads::detect_cpu_model()),
        _ => String::new(),
    }
}

/// Measured cost of one passage through the embed worker on this machine's
/// NPU: a 512-chunk run held 31.6-32.1 chunks/s at group sizes 4, 8, 16 *and*
/// 64 alike. The per-chunk cost is flat, so batching past a handful of
/// passages buys no throughput and only makes one group longer.
const EMBED_CHUNK_COST_MS: usize = 31;

/// Longest wait the scheduler promises a queued higher-class request: the
/// cross-class hard escape fires once an inflight has run past
/// [`crate::aiplane::scheduler::HARD_ESCAPE_THRESHOLD`], detected at
/// [`crate::aiplane::scheduler::HARD_ESCAPE_TICK`] resolution. Pinned to both
/// by `escape_budget_matches_the_scheduler`, so changing either scheduler
/// constant without sizing the embed group against it fails the suite.
const HARD_ESCAPE_BUDGET_MS: usize = 250;

/// Smallest group worth dispatching as a default. One passage per call would
/// double the round trips of every pass to save nothing: a single chunk is
/// already non-preemptible, so the extra IPC buys no extra responsiveness.
const EMBED_GROUP_MIN: usize = 2;

/// Passages per embedding round trip, on either route below.
///
/// Bounded on purpose — but the reason changed with BUG-20260927-2010. A
/// batch used to reach `AiplaneDispatch::batch` with nothing in front of it,
/// so this constant was the *only* thing standing between a 25 MB Telegram
/// export and a foreground `knowledge_search`. Scheduler admission does that
/// job properly now: caps, class queues and strict priority apply to every
/// group, on both routes.
///
/// What admission still cannot do is take a group back *mid-flight*. An ONNX
/// call is not interruptible, so the hard-escape watchdog's cancel only lands
/// once the group has returned — the caller sees success, never
/// `Cancelled`, and the adaptive shrink never fires in the field. So **this
/// constant, not the watchdog, is the real bound on how long a foreground
/// search waits behind an index pass**, and it is sized to the budget the
/// scheduler advertises: 8 passages × [`EMBED_CHUNK_COST_MS`] ≈ 250 ms =
/// `HARD_ESCAPE_THRESHOLD` + one `HARD_ESCAPE_TICK`, enforced by
/// `one_group_fits_the_hard_escape_budget`. The 64 this replaced cost
/// ≈ 2 s per group — and ≈ 2 s is exactly what a search measured
/// waiting behind a 512-chunk pass. Throughput does not pay for the shrink
/// (31.6-32.1 chunks/s at every size from 4 to 64), and a smaller group is
/// also a smaller JSON body over the socket.
pub const EMBED_IPC_MAX_CALL: usize = {
    let within_budget = HARD_ESCAPE_BUDGET_MS / EMBED_CHUNK_COST_MS;
    if within_budget > EMBED_GROUP_MIN {
        within_budget
    } else {
        EMBED_GROUP_MIN
    }
};

/// QoS class for bulk embedding (SPEC §4.3). A batch is bulk work by
/// definition — the latency-sensitive path is [`embed_one`], whose single
/// query goes in at the caller's priority — so passes admit at `Background`.
/// That is what lets a search outrank and, past the escape threshold, preempt
/// an index pass instead of sharing the device with it unmediated. Asking for
/// `Interactive` here would put a 700-file pass in the same class as the
/// searches it exists to defer to.
const EMBED_BATCH_PRIORITY: sy_core::Priority = sy_core::Priority::Background;

/// Floor for the adaptive group size. One chunk embeds in
/// [`EMBED_CHUNK_COST_MS`], inside
/// [`crate::aiplane::scheduler::HARD_ESCAPE_THRESHOLD`] (200 ms), and the
/// watchdog only preempts an inflight that has run past that threshold — so a
/// one-chunk group is not preemptible. That is the progress guarantee: if a
/// preemption ever does fire, a pass degrades to single-chunk dispatch and
/// still finishes, instead of livelocking by always being killed.
const EMBED_GROUP_FLOOR: usize = 1;

/// Group size to retry after the scheduler handed our slot to higher-priority
/// work: halve, floored. Bounded by `log2(EMBED_IPC_MAX_CALL)` steps.
fn shrink_group(current: usize) -> usize {
    (current / 2).max(EMBED_GROUP_FLOOR)
}

/// Whether a failed group is worth retrying smaller or ends the pass.
#[derive(Debug)]
enum GroupFailure {
    /// The scheduler preempted us (cross-class hard escape) or the class queue
    /// was full. Our work was not wrong, merely outranked — so the response is
    /// to come back smaller and quieter, not to fail the file.
    Preempted,
    /// Plane down, worker broken, output malformed: say so and stop.
    Fatal(KnowledgeError),
}

impl GroupFailure {
    fn classify(e: AiplaneError) -> Self {
        match e {
            AiplaneError::Cancelled | AiplaneError::Overloaded { .. } => Self::Preempted,
            other => Self::Fatal(plane_unreachable(format!("{other:#}"))),
        }
    }
}

/// Embed a batch of indexed passages. Each output vector is
/// L2-normalised. Adds the E5 `passage: ` prefix.
///
/// Two routes, chosen by who owns `/dev/accel/accel0`, and **both** go through
/// the scheduler:
/// * this process *is* the plane (`sy knowledge daemon` and its passes) —
///   [`crate::aiplane::ipc::admit_blocking_batch`], which admits to the
///   in-process `Background` queue;
/// * otherwise (`sy knowledge index`, `add`, `bench`) — `aiplane.batch` over
///   IPC in [`EMBED_IPC_MAX_CALL`]-sized groups, admitted by the daemon.
///
/// There is deliberately no third route. Embedding locally from a CLI process
/// means a second ORT session on a single-context device, which either fails
/// outright or silently degrades to the CPU EP and mislabels a minutes-long
/// pass as an hours-long one (the `sy knowledge sync` warning, and
/// `BUG-20260927-0910`'s memory storm, are the same mistake seen from the
/// other side). Before the IPC route existed the second route was missing
/// entirely: the CLI demanded an in-process supervisor nobody installs and died
/// with "aiplane supervisor not running", plane up or not.
///
/// Groups are retried at [`shrink_group`] size when the scheduler preempts
/// them, which it now may: routing a pass through admission is what makes it
/// preemptible in the first place.
pub fn embed_batch(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let inputs: Vec<WorkloadInput> = texts
        .iter()
        .map(|t| WorkloadInput::Text {
            text: format!("passage: {t}"),
        })
        .collect();
    // Who owns the device picks the route; the caller never has to know.
    let plane_here = supervisor::current();

    let mut out = Vec::with_capacity(texts.len());
    let mut group = EMBED_IPC_MAX_CALL.min(inputs.len());
    let mut start = 0usize;
    while start < inputs.len() {
        let end = (start + group).min(inputs.len());
        let slice = &inputs[start..end];
        let dispatched = match plane_here {
            Some(ref sup) => admit_or_direct(sup, slice),
            None => crate::aiplane::ipc::batch_blocking(
                WorkloadKind::Embed,
                slice.to_vec(),
                EMBED_BATCH_PRIORITY,
            ),
        };
        match dispatched {
            Ok(outputs) => {
                out.extend(vectors_from(outputs)?);
                start = end;
            }
            Err(e) => match GroupFailure::classify(e) {
                GroupFailure::Preempted if group > EMBED_GROUP_FLOOR => {
                    group = shrink_group(group);
                }
                GroupFailure::Preempted => return Err(preempted_at_floor().into()),
                GroupFailure::Fatal(ke) => return Err(ke.into()),
            },
        }
    }
    Ok(out)
}

/// Admit one group through the scheduler, falling back to a direct supervisor
/// call only when no scheduler is installed yet (worker tests, the moment
/// before the bridge comes up) — the same documented fallback
/// [`embed_one`] uses. The direct call used to be the *only* route for the
/// plane's own process, which is how a daemon index pass came to run outside
/// the class queues entirely (BUG-20260927-2010).
fn admit_or_direct(
    sup: &std::sync::Arc<supervisor::Supervisor>,
    group: &[WorkloadInput],
) -> std::result::Result<Vec<WorkloadOutput>, AiplaneError> {
    crate::aiplane::ipc::admit_blocking_batch(
        WorkloadKind::Embed,
        group.to_vec(),
        EMBED_BATCH_PRIORITY,
    )
    .or_else(|e| match e {
        AiplaneError::WorkloadFailed(ref inner)
            if inner.to_string().contains("scheduler not running") =>
        {
            sup.run_batch(WorkloadKind::Embed, group.to_vec())
                .map_err(AiplaneError::WorkloadFailed)
        }
        other => Err(other),
    })
}

/// Even the non-preemptible floor got preempted: not a contention accident
/// worth retrying, but a plane that is being cancelled under us.
fn preempted_at_floor() -> KnowledgeError {
    KnowledgeError {
        code: exit::EMBEDDING_FAILED,
        msg: "embed batch: every group down to one chunk was preempted by               higher-priority work — another caller is holding the NPU; retry               once the plane is idle (`sy aiplane status`)"
            .into(),
    }
}

/// Why a process that does not own the NPU could not embed, and what to do
/// about it. Kept separate from the call site so the operator-facing text is
/// assertable without standing up (or faking) a socket.
fn plane_unreachable(reason: impl std::fmt::Display) -> KnowledgeError {
    KnowledgeError {
        code: exit::EMBEDDING_FAILED,
        msg: format!(
            "embed batch: {reason}; embedding belongs to the NPU worker — start the plane \
             with `systemctl --user start sy-knowledge.service` and retry"
        ),
    }
}

/// Unwrap `WorkloadOutput::Vector` positionally. A wrong variant is fatal
/// here: callers map `vector[i]` → `chunk[i]` when they build qdrant points.
fn vectors_from(outputs: Vec<WorkloadOutput>) -> Result<Vec<Vec<f32>>> {
    let mut out = Vec::with_capacity(outputs.len());
    for o in outputs {
        match o {
            WorkloadOutput::Vector { vector } => out.push(vector),
            other => {
                return Err(KnowledgeError {
                    code: exit::EMBEDDING_FAILED,
                    msg: format!("embed: unexpected output {other:?}"),
                }
                .into());
            }
        }
    }
    Ok(out)
}

/// Embed a single search query (used by `sy knowledge search` and
/// the MCP server). The worker applies the E5 `query: ` prefix when
/// none is present in the caller's input.
///
/// `priority` controls scheduler admission (SPEC §4.3): foreground
/// search defaults to `Interactive`; the daemon's own background
/// passes pass `Background`. When the bridge's scheduler isn't yet
/// running (CLI fallback path, bench, fresh boot), falls back to a
/// direct `Supervisor::run_batch` — the priority is ignored in that
/// path because there's nothing to schedule against.
pub fn embed_one(text: &str, priority: sy_core::Priority) -> Result<Vec<f32>> {
    let input = WorkloadInput::Text {
        text: text.to_string(),
    };
    match crate::aiplane::ipc::admit_blocking(WorkloadKind::Embed, input.clone(), priority) {
        Ok(WorkloadOutput::Vector { vector }) => return Ok(vector),
        Ok(other) => {
            return Err(KnowledgeError {
                code: exit::EMBEDDING_FAILED,
                msg: format!("embed: unexpected output variant {other:?}"),
            }
            .into());
        }
        Err(crate::aiplane::error::AiplaneError::WorkloadFailed(e))
            if e.to_string().contains("scheduler not running") => {}
        Err(e) => {
            return Err(KnowledgeError {
                code: exit::EMBEDDING_FAILED,
                msg: format!("embed worker: {e}"),
            }
            .into());
        }
    }
    // Scheduler not installed — direct supervisor call as fallback.
    let sup = require_supervisor("embed")?;
    let outputs = sup
        .run_batch(WorkloadKind::Embed, vec![input])
        .map_err(|e| KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("embed worker: {e:#}"),
        })?;
    match outputs.into_iter().next() {
        Some(WorkloadOutput::Vector { vector }) => Ok(vector),
        Some(other) => Err(KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("embed: unexpected output variant {other:?}"),
        }
        .into()),
        None => Err(KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: "embed: worker returned empty batch".into(),
        }
        .into()),
    }
}

fn require_supervisor(call: &str) -> Result<std::sync::Arc<supervisor::Supervisor>> {
    supervisor::current().ok_or_else(|| {
        KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("{call}: aiplane supervisor not running"),
        }
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A group is the unit of work the scheduler *cannot* take back: an ONNX
    /// call is not interruptible mid-flight, so the hard-escape watchdog's
    /// cancel only lands once the group returns. Group size is therefore the
    /// real bound on how long a foreground search waits behind an index pass
    /// (BUG-20260927-2010 residual), and it must stay inside the escape budget
    /// the scheduler promises -- otherwise `HARD_ESCAPE_THRESHOLD` is a number
    /// on paper and the observed stall is one whole group (~2 s at the old
    /// 64-chunk bound, which is what a search measured during a 512-chunk
    /// pass).
    #[test]
    fn escape_budget_matches_the_scheduler() {
        use crate::aiplane::scheduler::{HARD_ESCAPE_THRESHOLD, HARD_ESCAPE_TICK};
        assert_eq!(
            HARD_ESCAPE_BUDGET_MS,
            (HARD_ESCAPE_THRESHOLD + HARD_ESCAPE_TICK).as_millis() as usize,
            "the embed group is sized against the escape budget; if the \
             scheduler's promise moved, size the group again"
        );
    }

    #[test]
    fn one_group_fits_the_hard_escape_budget() {
        let budget_ms = HARD_ESCAPE_BUDGET_MS;
        let group_ms = EMBED_IPC_MAX_CALL * EMBED_CHUNK_COST_MS;
        assert!(
            group_ms <= budget_ms,
            "one embed group costs {group_ms} ms at {EMBED_CHUNK_COST_MS} ms/chunk \
             over the {budget_ms} ms escape budget: a search queued behind it \
             waits that long (EMBED_IPC_MAX_CALL={EMBED_IPC_MAX_CALL})"
        );
    }

    /// The no-plane path must say *what to do*, not just that an internal
    /// invariant broke: `sy knowledge index` used to fail with
    /// "aiplane supervisor not running" and no pointer to the plane.
    /// The floor is what makes the retry a progress guarantee rather than a
    /// spin: halving must land on it and stay there, because a one-chunk group
    /// finishes inside the watchdog's escape threshold and therefore cannot be
    /// preempted at all.
    #[test]
    fn group_shrinks_to_the_non_preemptable_floor() {
        let mut g = EMBED_IPC_MAX_CALL;
        let mut ladder = Vec::new();
        for _ in 0..3 {
            g = shrink_group(g);
            ladder.push(g);
        }
        assert_eq!(ladder, vec![4, 2, 1], "halve per preemption");
        let mut floor = EMBED_GROUP_FLOOR;
        for _ in 0..4 {
            floor = shrink_group(floor);
        }
        assert_eq!(floor, EMBED_GROUP_FLOOR, "floor must be stable");
    }

    /// Being outranked is not a failure: a preempted group comes back smaller.
    /// A dead worker or a malformed plane is, and must reach the operator with
    /// the actionable "start the plane" text instead of being retried forever.
    #[test]
    fn preemption_retries_smaller_and_worker_failure_is_fatal() {
        assert!(matches!(
            GroupFailure::classify(AiplaneError::Cancelled),
            GroupFailure::Preempted
        ));
        assert!(matches!(
            GroupFailure::classify(AiplaneError::Overloaded {
                class: sy_core::Priority::Background,
                queue_depth: 256,
                retry_after_ms: 200,
            }),
            GroupFailure::Preempted
        ));
        match GroupFailure::classify(AiplaneError::WorkloadFailed(anyhow::anyhow!(
            "ORT session crashed"
        ))) {
            GroupFailure::Fatal(ke) => {
                assert_eq!(ke.code, exit::EMBEDDING_FAILED);
                assert!(ke.msg.contains("ORT session crashed"), "{ke:?}");
                assert!(ke.msg.contains("sy-knowledge.service"), "{ke:?}");
            }
            other => panic!("a worker failure must be fatal, got {other:?}"),
        }
    }

    #[test]
    fn plane_unreachable_names_the_fix() {
        let err = plane_unreachable("aiplane daemon not running: connect /run/user/0/x.sock");
        let msg = err.to_string();
        assert!(
            msg.contains("aiplane daemon not running")
                && msg.contains("systemctl --user start sy-knowledge.service"),
            "the error must carry the condition and the fix, got {msg}"
        );
    }

    #[test]
    fn vectors_from_keeps_positional_order_and_rejects_other_variants() {
        let outs = vec![
            WorkloadOutput::Vector { vector: vec![1.0] },
            WorkloadOutput::Vector { vector: vec![2.0] },
        ];
        let got = vectors_from(outs).expect("two vectors");
        assert_eq!(got, vec![vec![1.0f32], vec![2.0f32]]);

        let bad = vectors_from(vec![
            WorkloadOutput::Vector { vector: vec![0.5] },
            WorkloadOutput::Text {
                text: "nope".into(),
            },
        ])
        .expect_err("a non-vector output must fail the batch");
        assert!(bad.to_string().contains("unexpected output"));
    }
}
