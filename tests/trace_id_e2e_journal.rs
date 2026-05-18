//! End-to-end trace_id propagation through the journald sink.
//!
//! SPEC §4.6 / arch-observability Step 4 DoD bullet #2:
//! `journalctl --user -u 'sy-*' SY_TRACE_ID=<id> -o json` stitches a
//! real call chain on the rice. This requires:
//!
//! 1. A user-mode systemd session (`loginctl show-user $UID`).
//! 2. The `sy-aiplane.service` user unit running.
//! 3. A FakeWorkload available so `sy aiplane run --workload fake`
//!    doesn't reach `/dev/accel/accel0`.
//!
//! None of the above hold inside a sandboxed test runner, so this
//! test stays `#[ignore]`. Run by hand on the rice with:
//!
//! ```bash
//! cargo test --test trace_id_e2e_journal -- --ignored
//! ```
//!
//! The recipe the test would execute:
//!
//! 1. Mint a fixed `TraceId` (call it `T`).
//! 2. `sy aiplane run --trace-id $T --workload fake --priority Interactive -- '{"sleep_ms":50}'`.
//! 3. `journalctl --user -u sy-aiplane SY_TRACE_ID=$T -o json --since=-1min`.
//! 4. Assert the result is a non-empty JSON array (one record per
//!    log line the daemon emitted while handling the request).
//!
//! The `SY_TRACE_ID` field name is journald's uppercased form of
//! the `trace_id` tracing field; `tracing-journald` uppercases all
//! recorded fields by default. SPEC §4.6 specifies the uppercase
//! prefix so journalctl's indexed-field filter works.

#[test]
#[ignore = "requires a user-mode systemd session + sy-aiplane.service + a FakeWorkload; run by hand on the rice"]
fn trace_id_stitches_journal_chain() {
    // The body is the manual recipe; running it inside the test
    // harness would shell out to `journalctl` which isn't
    // hermetic. The Step 4 DoD treats this as documentation, not
    // a CI gate — the unit tests in `sy_core::obs::tests` and
    // `sy_ipc::server::tests` cover the propagation contract
    // end-to-end at the in-process level.
    panic!("ignored — see test docstring for the manual rice recipe");
}
