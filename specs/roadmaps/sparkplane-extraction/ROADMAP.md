# ROADMAP: Sparkplane extraction

Source: `specs/journeys/JOURNEY-20260921-sparkplane.md` and approved conversation proposal.

## Overview

Extract the audited working tree, not an older commit. Keep the live optimized deployment unchanged until migration qualification succeeds. Original source is retained until its independent replacement and bridge pass verification.

## 1 — Standalone extraction

Tests: independent Cargo metadata, CLI subprocess help/version, migrated Spark/IPC regression suites.

- [x] Preserve source provenance and asset hashes; exclude credentials and operational artifacts.
- [x] Own minimal IPC/core support; no sy dependencies or desktop/NPU dependencies.
- [x] Client/appliance build and test independently.

## 2 — Canonical namespace and releases

Tests: existing signed inventory, policy, engine and API contracts adapted to Sparkplane; namespace and packaging checks.

- [x] Canonical paths, command names, schema identifiers, policies and Docker ownership.
- [x] Independent CI, signed artifacts and developer/operator documentation.
- [x] Optimized vLLM profile and GPU device mapping retained.

## 3 — Bridge and pinned distribution

Tests: fake-executable process boundary, binary/signature corruption, disabled integration, dry-run, atomic installation, environment precedence.

- [x] Raw argument forwarding before sy NPU/runtime initialization.
- [x] Optional pinned signed installation through sy apply, no command-time downloads.
- [x] Remove duplicate Spark implementation and exclusive dependencies only after parity gates pass.

## 4 — Safe migration

Tests: dry-run, conflicts, wrong signer, stage interruption, SQLite backup and rollback, generated-file ownership, exact-container lifecycle.

- [x] Old-authority-signed trust transition to new project authority.
- [x] Journaled typed state migration, same-filesystem cache rename, preserved UID/GID and credentials.
- [x] Fail closed and restore before accepting new mutations; no stale snapshot restoration after commit.

## 5 — Publication and cutover

Tests: clean-checkout builds and install; current-device full-context, streaming, tools, cancellation, concurrency and matched performance checks.

- [x] Publication audit and public GitHub repository populated.
- [x] Repository release protection and independent authority provisioned.
- [x] Live migration dry-run reviewed and cutover verified; extra failure-recovery reloads documented below.
- [x] No active old namespaces; evidence retained.

## Cross-cutting Definition of Done

- [x] sy default lint/test and independent Sparkplane client/appliance lint/test pass locally.
- [x] All journey acceptance criteria met; no claim of completion before live/release gates.
- [x] Docs describe tested procedures and retained historical diagnostics explicitly.

## Implementation checkpoint — 2026-09-21

Public source: <https://github.com/Sumatoshi-tech/sparkplane>. Sparkplane uses
its own pinned Rust 1.95.0 workspace, client/appliance gates and signed-release
workflow. Its README and operator docs describe the product alone; the user
explicitly rejected extraction/comparison narrative there. Integration contracts
and unfinished migration work remain here in `CONTRACTS.md` and `MIGRATION.md`.

The sy bridge has 12 passing tests covering signatures, corrupted receipts,
atomic activation, idempotent offline apply, raw/non-UTF-8 arguments, exit codes,
environment precedence and execution without the sy checkout or AMD runtime.
All default sy workspace tests and lint passed. README preview goldens were
regenerated for the intentional documentation change. One initial io_uring
timing assertion failed during competing builds; the complete idle rerun passed.

The original 84 extracted implementation/assets/tests/workflow files are
preserved unchanged under `target/sparkplane-extraction-originals-20260921/`.
Historical specs and unrelated working-tree changes remain in sy. No sy commit
or installed workstation binary replacement has been performed.

Remaining: CI/release artifact qualification, dedicated signing authority,
complete journaled appliance runner and ownership-aware generated-client-file
migration, signed trust transition, real pinned client installation, and live
cutover acceptance. The live optimized vLLM deployment has not been changed.

## Continuation checkpoint — 2026-09-21

CI run `35545800923` and unsigned release-build run `35545801009` both passed.
Both client architectures and the ARM64 appliance built independently. The
downloaded x86-64 client ran outside either checkout; the ARM64 appliance passed
its read-only inspector on Spark. The exact temporary probe was removed. No live
service, engine, model, credential or workstation installation was replaced.
Main now requires a pull request, the `verify` check and resolved conversations;
force pushes and deletion are disabled, including for administrators.

Additional tested safeguards now include a locked, fsynced recovery journal with
ordered actions and a no-rollback commit boundary; inode-bound same-filesystem
directory moves; exact container identity verification; engine-setting equality
apart from owned namespace fields; preservation of operator resource policy and
numeric identity; compile-cache keys shared with the executor; immutable snapshot
path validation; and WAL-inclusive database staging that never publishes failed
conversion output. Fresh install now rejects an unmigrated appliance.

Generated client files now use digest ownership receipts. Launch and restore
preserve edits, reject symlinks/unowned conflicts, validate the complete pair
before mutation, and recover interrupted pair publication. Existing legacy
generated files remain untouched and are not silently adopted.

Both feature-mode lint gates and client tests pass. The full appliance/workspace
suite passed twice; dependency audit passed (transitive duplicate-version
warnings remain). These safeguards are not yet a complete host cutover runner:
traffic fencing/drain, concrete service/account/container actions, recovery
integration, final signed release/client pins and live qualification remain.
The existing encrypted signing key cannot be unlocked by this session; no
`MINISIGN_PASSWORD` or legacy repository signing secret is available. Operator
signing is required, never an unsigned replacement of the installed authority.

Safety work is published in <https://github.com/Sumatoshi-tech/sparkplane/pull/1>.
Its initial CI run `35548339618` passed. A follow-up reproduces and fixes an
encrypted-signing pipeline defect: exporting `MINISIGN_PASSWORD` alone does not
unlock Minisign 0.12. The release now feeds the password through stdin, verifies
payload checksums, publishes the public key, and exercises real encrypted signing
with a disposable fixture key in both CI modes. Production keys were not used.
Operator provisioning commands are in Sparkplane's developer guide under
“Provision the release authority”; the signing-key directory is separate from
the client configuration destination to avoid blocking its atomic import.
The follow-up CI run `35548776783` passed all gates, including encrypted signing
with the runner's native Minisign executable. Its installation comes from the
signed Ubuntu package repository; the Rust crate is not a CLI installer.
PR #1 is merged as `b4defa1c8edd8f6a8fa0bd9f3d8f5307cdc24f58`; the standalone
checkout is clean on main. Live status still reports healthy generation 8,
262144-token context and no restart suppression. No release signing keys or
release-environment secrets have been provisioned by the agent.

## Local continuation — 2026-09-21 (not deployed)

Main CI `35548922423` also passed. A fresh authenticated read still shows the
optimized Qwen instance healthy at generation 8 with 262144-token context and
no restart suppression. The release environment still has no signing secrets.

Local Sparkplane changes add a finite action orchestrator with failure injection
at every stage, recovery in reverse order, and committed-restart behavior that
only reopens traffic. A reproduced journal persistence bug is fixed: after a
failed write, the in-memory journal is poisoned and must be reopened before
any further host action. The step order now has one shared definition.

Fixed host-command mappings and persistent systemd drop-in guard helpers are
covered by tests, including conflicts and symlink rejection before partial
publication. These helpers are not yet connected to a complete privileged
appliance migration CLI. They must not be used as a manual cutover recipe.
Host integration, live qualification and the signed release/client installation
remain unfinished. No host changes, signing-key changes or publication of these
local changes have occurred.

## Host integration checkpoint — 2026-09-21

The previous helper-only checkpoint is superseded by the concrete appliance
runner in <https://github.com/Sumatoshi-tech/sparkplane/pull/2>, commit
`d46b725e9d6caf6c522da199dd7c85da2b669f8f`. The appliance-only bootstrap CLI
validates the installed-authority transition, exact host/release/executable,
catalog compatibility, resource reserve and container/network ownership before
fencing. It journals drain/baseline, service shutdown, WAL-inclusive snapshot,
same-filesystem database/data/cache moves, numeric account renames, publication,
activation and qualification. Source drift is checked before and after stopping
state writers. No production signing key or signature has been fabricated.

Acceptance now includes full configured context, separate reasoning content,
tool continuation, complete streams, cancellation drain, concurrency and a
three-sample matched decode median within 5%. Exact old container/network
cleanup precedes commit. Pre-commit recovery restores the original layout;
post-commit resume only completes activation, never restores stale state.
The operator procedure and signature prerequisites are in `MIGRATION.md`.

Tests exercise actual filesystem/WAL cutover and every partial database/cache
rename, recovery after account/service failures, inode and permission identity,
Docker lifecycle over a Unix socket, pinned HTTPS, signature/payload tampering,
CLI feature boundaries and conflicting modes. Sparkplane `make lint`, `make
test`, `make test-client` and `make audit` pass locally; duplicate-version audit
warnings remain. PR CI `35566554105` passed and PR #2 is merged as
`d3981a714ffe71490e86168cdf424a896d6acb64`. Merged-main CI `35566808538`
also passed. Independent unsigned distribution build `35566869676` passed its
contract, x86-64 client and ARM64 client/appliance jobs; publication was correctly
skipped for a manual, untagged build.

Downloaded artifacts passed their payload checksums. The x86-64 client ran
outside either checkout, advertised `sparkplane.bridge/v1`, and dry-ran the real
36-file client import with `source_preserved: true`. The ARM64 appliance ran as
the ordinary SSH user on Spark, passed its content-addressed read-only inventory
probe and exposed all four mutually exclusive migration modes in CLI help.
The temporary remote probe and its empty directory were removed; the downloaded
artifact remains available locally. No unsigned privileged action was executed.

Smoke-test binary SHA-256 values (not deployment authority):

- x86-64 client: `0152498443d164f4d20fe6efb2e78f148a2c5789bb3cc420157c9e2d0ce70d1f`.
- ARM64 appliance: `d9be224a2f9b7380a97af812480eacb232248adb8fbe09b22431149b7fd9c271`.

The eventual signed release must be verified and pinned from its actual
published bytes, independently of these unsigned smoke-test artifacts.

The sy bridge now has 14 passing tests, including real PTY descriptor/PID
preservation and signal termination through the actual sy executable. The
bridge gate, default lint and formatting checks pass. Full sy tests exposed a
plugin fixture's process-global environment race; the deterministic regression
and process-isolation fix are documented in `specs/bugs/BUG-20260921-0903.md`.
After the fix, the full workspace suite passed twice. These sy changes remain
local and uncommitted.

A fresh live read still reports healthy Qwen generation 8, 262144 context and no
restart suppression. Read-only SSH confirmed that the service actually owns
UID 996 and GID 983; the runner discovers and preserves both independently.
No live control-plane, model or installed client change has occurred.

Remaining external gates: provision the dedicated protected signing secrets,
publish the signed release, unlock the installed authority to sign the exact
host-bound transition, review the real dry-run, perform live GPU acceptance and
activate the signed workstation client/bridge pins. GitHub's release environment
still contains no signing secrets. This is not a completed live migration.

## Autonomous signing and live preflight — 2026-09-21

The earlier signing blocker was incorrect: direct inspection of the current
legacy key's encryption marker and public-key derivation verified an unencrypted
key whose public key exactly matches the root-owned installed authority on Spark.
No password guessing, key replacement or signature bypass was needed.

A dedicated encrypted Sparkplane authority is now provisioned in GitHub's
protected `release` environment. Its local key is under
`~/.config/sparkplane-release-signing/`; the random unlock credential is encrypted
using user-scoped systemd credentials in `password.cred`, not stored as plaintext.
Both encrypted files are mode 0600 in a mode 0700 directory. The protected
maintainer-review and tag-only deployment rules remain unchanged.

The legacy key signed a finite, host-bound transition for an independently
verified CI build, which the new authority also signed. The root-owned executable
on Spark matched its exact approved digest before the real dry-run. No service
was stopped. The first preflight found two orphaned August 27 embedding serve
operations; both were cancelled through the supported API after verifying that
their target instance was stopped/absent and owned no running container.

The next preflight reproduced a historical-instance bug, documented in
`specs/bugs/BUG-20260921-1004.md`. Release run `35570685522` was cancelled before
publication; its `v0.1.0` tag remains immutable. The fix preserves stopped
history while retaining exact running-engine checks, and is being verified for
version 0.1.1. Signing is no longer an external blocker; live cutover is still
pending successful preflight and qualification.

The historical-state fix is merged in PR #3 (`cfcac99f3d1c903086d5a16d196aa635ccda573a`);
PR #4 adds the production `unless-stopped` lifecycle contract with policy-drift
rejection. Its CI `35573441265` passed. The real signed root dry-run now reports
`planned`, preserving generation 8, 262144 context, image
`sha256:fa6008389ff17911099e649aeb84b225e71e529c448aa46814ef397dc557f5ff`,
UID 996/GID 983 and the exact warm-cache relocation. No model reload or service
stop has happened yet; published-release verification and live qualification
remain mandatory.

The workstation now supports `sy apply --only sparkplane` (also
`SY_APPLY_ONLY=sparkplane`) with structured JSON/dry-run output. This avoids an
unrelated package upgrade found during the general apply preview. Two generated
Python cache files left in the old Spark config tree were moved to the existing
extraction recovery archive. The bridge has 17 passing tests; an executable
writer lifetime regression is recorded in `BUG-20260921-1035.md`. Default lint,
feature-minimal lint, formatting, full workspace tests twice and bridge tests
pass. The rebuilt sy binary remains staged, not installed.

PR #4 is merged as `884ba1abc27bae727dd41158d4616d0b2d0fe39e`; tag `v0.1.1`
starts protected release run `35573847722`. The standalone checkout is clean on
main. The first complete live dry-run evidence is root-private at
`/var/tmp/sparkplane-preflight-FSMh9O/plan.json`; its signed local build is only
preflight evidence, not a substitute for verifying the published release bytes.

The v0.1.1 release contract passed, but build setup omitted the pinned Rust
components and rustup failed with a `cargo-fmt` file conflict before compilation.
PR #5 adds explicit build components and a failing-before/passing-after regression
test. Version 0.1.2 preserves the immutable failed tag. Local lint, appliance and
client tests, and dependency audit pass; PR CI `35574916993` is running.
The failure and fix are recorded in `BUG-20260921-1049.md`. Production remains
healthy and unchanged while the signed publication gate is completed.

PR #5 merged as `d5fd2734bb55a9125530ded6a2335fa5af49b52b`. Protected release
run `35575371377` passed every job and published signed v0.1.2. Both inventories
and every payload verified against the independently provisioned public key.
The actual published ARM64 binary (`4fad6e61a107b51d8e0c4327918d0a04503d1e5805864a3f79dc52e1424c61c0`)
passed root-protected dry-run with an exact old-authority-signed transition.

The first live transaction passed full 262144-token inference, then rejected a
valid native `reasoning` response because the qualification probe recognized
only `reasoning_content`. Its journal remained at drain: no service stop,
account/data move or engine reload occurred. Supported recovery returned
`recovered` and reopened traffic. The source engine remained healthy. BUG
`BUG-20260921-1118.md` records the one-line compatibility fix and failing-before
pinned-HTTPS regression. Live tool call, continuation, 256-token SSE usage and
terminal `[DONE]` checks passed independently. A v0.1.3 patch release is needed
before retrying the complete migration; signing and publication are working.

PR #6 passed CI `35577471014` and merged as
`b4e2ab779448e25d767b38c89ce5946be2853159`; v0.1.3 release run `35577817644`
is in progress. Local lint, appliance/client tests, dependency audit and a second
full appliance suite all pass. Root read-only checks confirm the original two
services and exact generation-8 container stayed active after recovery, with no
remaining migration firewall table. No model reload has been consumed.

### Live v0.1.3 cutover and recovery checkpoint, 2026-09-21 12:25 MSK

Published v0.1.3 passed the full baseline, including 262144 context and median
33.8405 decode tokens/s. Activation then exposed installer umask and emergency
journal conversion bugs before any new-namespace engine started. PR #7 adds
explicit public-code permissions and reversible, evidence-validated emergency
schema conversion. The supported rollback restored the original layout and
data, but repeated 90-second agent startup timeouts exhausted the source restart
budget and removed the old exact container. Inference is currently unavailable;
traffic remains fenced and the original transaction is preserved.

After resource admission returned zero swap-in/PSI, a normal managed serve
requested generation 9 with the identical optimized image/settings. It is loading
normally. Recovery's original executable has reached its readiness deadline;
it did not reopen traffic. Regression-tested follow-up work adds the correct
startup timeout and a signed, exact-host/record/executable/container recovery
handoff restricted to the post-restoration checkpoint. No journal/database edits,
confinement bypass, Docker restart or reboot were used. Live migration and client
activation remain incomplete; do not interpret earlier green build gates as
completed deployment.

### Completed live migration and client activation — 2026-09-21

PR #7 merged as `cf2f2ac036a0f19ba1d9aa46d7d55a6e673a934d`; required CI
`35583113112` and protected v0.1.4 release `35583419070` passed. The release
was verified against the independent authority before any privileged execution.
Published client SHA-256:
`523979379b739e89416cc28b2e8df3c37279051c9193bc1492ad4df4af1f2867`.
Published ARM64 appliance SHA-256:
`fd7d333f966d48c919d746aae0919b5803d430db0d1b25ceeadb3527fe51d802`.

An installed-authority-signed approval bound the immutable v0.1.3 record,
replacement generation 9 and exact v0.1.4 recovery executable. Recovery returned
`recovered` without journal/database edits. The fresh v0.1.4 dry-run passed, then
the actual transaction completed every step and returned `committed`, exit 0.
Full-context inference consumed 262136 prompt plus 8 completion tokens. Separate
reasoning, tool calls and continuation, complete SSE, concurrency and cancellation
all passed. Matched decode medians were 33.808257809150184 before and
33.803972117847486 after (−0.0127%). The exact optimized image and settings remain
unchanged. Generation 9, model identity, UID 996/GID 983, credentials, TLS and
cache/data identities are preserved.

`sparkplane-agent` and `sparkplane-executor` are active at 0.1.4, with zero service
restarts. `sparkplane.target` is enabled; old services are inactive and the old
target disabled. Exact old container/network cleanup completed before commit;
only the Sparkplane-managed engine and internal network remain. The traffic
fence and temporary unit guards are removed. No Docker restart or reboot occurred.
The initial failed cutover and rollback caused additional recovery reloads; the
single-reload happy path was not achieved on this host and is not claimed.

The 36-file client import preserved its source. `sy.toml` pins the verified
v0.1.4 client; scoped apply passed and its second run was idempotent. The rebuilt
sy binary was atomically installed after retaining its predecessor under
`target/sparkplane-client-cutover-20260921/sy.previous`. A standalone command link
at `~/.local/bin/sparkplane` targets the managed client directly. Both entrypoints
work from `/tmp`, without either checkout. Status has no degraded reasons;
the only running model is healthy Qwen3.8, context 262144, no suppression.
Codex configuration reused that instance and the migrated inference token;
generated files have digest ownership receipts. An external pinned-HTTPS
Responses stream using that inference credential completed with `OK`.

Doctor retains the accepted shared-bridge risk and suppression diagnostics for
the two stopped historical qualification instances; neither runs or affects the
production instance. Historical records were not cleared to hide these warnings.
The original failed transaction and all migration evidence remain root-private;
the committed transaction must never be recovered over accepted new work.
Both projects' lint/test gates, client-only checks and Sparkplane dependency audit
passed. Sparkplane is published on protected main; sy edits remain local and
uncommitted, preserving the user's other working-tree changes.
