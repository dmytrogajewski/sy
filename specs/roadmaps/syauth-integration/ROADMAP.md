# ROADMAP: syauth integration — operator-facing surface in `sy`

Source: `~/sources/syauth/specs/syauth/SPEC.md` (canonical syauth spec) and `~/sources/syauth/docs/known-gaps.md` (closed deviation rows DEV-001..DEV-004).

## Overview

syauth is a phone-as-key Linux PAM unlock module. The desktop ships
two binaries (`syauth` CLI + `pam_syauth.so`) plus a long-running
user daemon (`syauth-presenced`); the phone ships an Android app that
holds the LESC-bonded Ed25519 keypair inside the Keystore (DEV-002
closure). syauth on its own is a CLI + a PAM shared object + a
daemon: the operator-facing surfaces (status pill in the bar, LESC
numeric-comparison popup, accept/reject buttons, install helpers)
belong in `sy`. This roadmap lands them in five ordered,
independently shippable steps.

End state: `sy syauth` is the one operator-visible touchpoint —
waybar pill renders bond + connection state, the LESC 6-digit code
appears in the bar during pair, `sy syauth install-pam <service>`
wires `pam_syauth.so` into the PAM stack with the right control
flag, and `sy syauth list / revoke` mirror the underlying CLI for
in-bar discoverability. The syauth crate stays the source of truth
for protocol + crypto; `sy` only renders + orchestrates.

Today (commit at top of master): `src/syauth.rs` already ships the
waybar pair-request applet (file-IPC at
`${XDG_RUNTIME_DIR}/syauth/`), `sy syauth --waybar` returns the JSON
the bar expects, and `sy syauth accept|reject` writes the response
file. The bar's `custom/sy-syauth` slot is wired in
`configs/waybar/config.jsonc:83-90` with a 1s poll interval. Steps
1–4 grow that surface; step 5 documents the operator setup flow.

Pre-flight check before step 1 (verified 2026-05-19 against
`~/sources/syauth` HEAD):

- `~/sources/syauth/target/debug/syauth --help` lists `pair / list
  / revoke / status / install-pam / uninstall-pam / install-presenced
  / doctor`.
- `syauth pair` accepts `--force` (added during DEV-001..DEV-003
  closure on 2026-05-17).
- `syauth status --json` and `syauth doctor --json` both emit
  typed JSON (machine-readable surface for the bar + the
  `sy syauth doctor` shim in step 5).
- `syauth install-pam` hard-codes the auth-line control flag
  to `required` (`crates/syauth-cli/src/install_pam.rs:38`,
  `const CONTROL_FLAG: &str = "required"`). Step 3 owns the
  upstream `--control` patch that lets the operator pick
  `sufficient`.
- Desktop bond schema is **v1** (`BOND_SCHEMA_VERSION_LATEST = 1`
  in `crates/syauth-core/src/bond.rs:68`); the on-disk
  `/var/lib/syauth/bonds.toml` carries `peer_id` + `pubkey`
  per bond. Schema **v2** with `keystore_alias` +
  `phone_pubkey_hex` is the Android-local file
  (`syauth-android/app/src/main/kotlin/.../bond/BondStore.kt:56-57`,
  stored at `filesDir/syauth-bond.toml`); the two schemas
  evolve independently and the roadmap does not bridge them.
- An end-to-end unlock (sudo → pam_syauth.so → daemon → phone
  biometric → response) was verified 2026-05-19 against a real
  Galaxy S25 Ultra. The minimum viable PAM line on Fedora 43 is
  `auth sufficient pam_syauth.so socket=/run/user/<uid>/syauth/auth.sock
  timeout=8000`. Two reality-corrected constants for step 3:
  - `timeout=8000` (8 s), not `1200` — real BiometricPrompt
    reaction is 2–3 s and `pam_syauth`'s recently bumped
    `DAEMON_RESPONSE_BUDGET` is 8000 ms; setting `timeout=1200`
    re-introduces the bug that fell every unlock through to
    FIDO.
  - control flag MUST be `sufficient` (not `required`), so the
    FIDO / password fallback survives when the phone is out of
    range or the biometric is denied.
- `/var/lib/syauth/last.log` interleaves two formats: CSV from
  the daemon (`peer_id,nonce,t_start_ms,t_end_ms,outcome,reason`)
  and ISO-timestamp lines from the PAM module
  (`<rfc3339> success|denied <peer_id>`). Any consumer that
  parses the log MUST filter on `NF == 6` before extracting
  latency columns (mirrors the fix in
  `~/sources/syauth/scripts/e2e-unlock.sh`).

---

## Step 1 — `sy syauth status` pulls live bond + adapter state from the syauth CLI

**Goal:** a single command renders the operator-visible source of
truth: is a phone paired, is the adapter up, did the last unlock
succeed or fail. Today the waybar applet only renders the pair
request; in idle state the bar pill says nothing about the bond.

**Files:**
- `src/syauth.rs::run` (modified) — dispatch a new `"status"`
  action that shells out to `syauth status --json` and renders a
  one-line summary, parsed from the JSON keys
  (`daemon.state`, `daemon.peers[0].peer_id`,
  `daemon.peers[0].last_connect_ms_ago`). Example output:
  `bonded 665d53… · adapter hci0 ok · daemon up · last unlock 4s ago ok`.
  Mirrors the format the bar pill needs in step 2.
- `src/syauth.rs::waybar_out` (modified) — when no pair request is
  pending, fall back to the same status-string renderer so the bar
  always has something to render instead of going blank.
- `src/syauth.rs::parse_status_json` (new) — pure fn taking a
  `&str` of `syauth status --json` output, returning a typed
  `StatusSummary { state, peer_id, last_connect_ms_ago, last_unlock_outcome }`.
  Lives next to the existing `read_request` helper.

**Tests:**
- New `src/syauth.rs::tests::status_renders_bonded_line` — feed a
  fixture `syauth status --json` snippet through `parse_status_json`
  and assert the one-liner ends with `bonded 665d53…`.
- `src/syauth.rs::tests::status_handles_empty_peers_array` — JSON
  with `daemon.peers = []` renders `not paired`.
- `src/syauth.rs::tests::status_handles_daemon_down` — JSON with
  `daemon.state = "down"` renders `daemon down` (no peer line) and
  exits non-zero.

**Definition of Done:**
- [ ] `sy syauth status` returns 0 when bonded + daemon up, 1
      otherwise, with the single-line output documented above.
- [ ] Waybar idle pill renders the status line instead of an
      empty slot.
- [ ] `make lint && make test` green; no new banned vocabulary in
      `src/syauth.rs`.

---

## Step 2 — Waybar pill shows live bond + connection state, not just pair requests

**Goal:** the bar pill is meaningful even when no pair is in
flight. Five visual states:

| State                        | Pill text                | Class      |
|------------------------------|--------------------------|------------|
| no bond                      | `syauth: not paired`     | `unpaired` |
| bonded + adapter on + idle   | `syauth: ✓ fedora`       | `ok`       |
| bonded + adapter off / lost  | `syauth: · fedora`       | `degraded` |
| pair request pending         | `syauth: 6-digit 000000` | `pending`  |
| unlock in flight             | `syauth: → fedora`       | `active`   |

The five states map to JSON fields the existing `syauth status
--json` already exposes:
- `unpaired` ← `daemon.peers == []`
- `ok` ← `peers[0].in_flight_challenges == 0 && daemon.state == "up"
  && bluez_adapter == "ok"` (the `bluez_adapter` field is in
  `syauth doctor --json`; the pill reads `doctor --json` once per
  poll, not `status --json`, so the adapter probe is in scope).
- `degraded` ← `daemon.state != "up" || bluez_adapter != "ok"`
- `pending` ← existing pair-request file path; pre-empts everything
- `active` ← `peers[0].in_flight_challenges > 0`

**Files:**
- `src/syauth.rs::waybar_out` (modified) — branch on pair-request
  presence first (existing path); otherwise call into the step-1
  status parser and render the table above.
- `configs/waybar/style.css` (modified) — add `.custom-sy-syauth.ok`
  / `.degraded` / `.active` / `.unpaired` style hooks. Today the
  file has none. The `.pending` hook does not exist either; add it
  alongside the new four.
- `configs/waybar/config.jsonc:86` (modified) — keep
  `"interval": 1` during pair (the existing 1 s value); the
  waybar `signal` field already covers on-demand wakes. Document
  this in a comment above the slot so future trimming doesn't
  accidentally raise it.

**Tests:**
- `src/syauth.rs::tests::waybar_renders_pending_when_request_present`
  — existing test, keep.
- `src/syauth.rs::tests::waybar_renders_ok_class_when_bonded_idle`
  — new; feeds a fixture `syauth status --json` with
  `in_flight_challenges = 0` and asserts the emitted JSON's
  `"class": "ok"` field.
- `src/syauth.rs::tests::waybar_renders_unpaired_when_no_bond`
  — new.
- `src/syauth.rs::tests::waybar_renders_degraded_when_daemon_down`
  — new.

**Definition of Done:**
- [ ] All five pill states render correctly against a fixture-
      backed `syauth status --json` (no live BlueZ adapter required
      in tests).
- [ ] Style hooks documented in `configs/waybar/style.css` near
      `custom-sy-syauth`.
- [ ] `make lint && make test` green.

---

## Step 3 — `sy syauth install-pam <service>` wraps the underlying CLI with the right control flag

**Goal:** the operator runs ONE command to make syauth the primary
auth on a PAM service with sensible defaults. The syauth CLI's
`install-pam` currently hard-codes `auth required pam_syauth.so` —
that's wrong for the "fall through when phone unavailable" semantic
the operator wants (a required-failing syauth blocks the stack even
when the phone is out of range and the user wants FIDO / password
instead). This step:

1. Upstream: patch
   `~/sources/syauth/crates/syauth-cli/src/install_pam.rs` to accept
   a `--control <flag>` argument, default still `required` for
   backward compatibility; accepts `sufficient` and the bracketed
   forms (`[success=done auth_err=die default=ignore]`). The
   constant `CONTROL_FLAG` becomes a `default_control()` helper;
   `build_line(opts)` consumes `opts.control`. Snapshot test
   updates `install_pam_help_snapshot.snap` for the new flag row.
2. `src/syauth.rs::run` — add an `"install-pam"` action that takes
   `--service` and `--control` (defaults to `sufficient`), shells
   out to `syauth install-pam --service X --control sufficient
   --module-args timeout=8000 --yes`, prints the resulting file
   diff to stdout, and reminds the operator to verify with
   `pamtester <service> $USER authenticate`.
3. `src/syauth.rs::run` — companion `"uninstall-pam"` action that
   shells out to `syauth uninstall-pam` (which restores from the
   `.bak` snapshot the CLI wrote).

Reality-corrected defaults (verified 2026-05-19): `--control
sufficient` and `--module-args timeout=8000` are the values that
gave a green e2e unlock on Fedora 43 with the Galaxy S25 Ultra.
`timeout=1200` (the upstream CLI's current default) is too tight
for real BiometricPrompt reaction time and times out every unlock.

**Files:**
- `~/sources/syauth/crates/syauth-cli/src/install_pam.rs` (modified
  in the syauth repo, cross-repo dependency — call out in the
  commit message). The `CONTROL_FLAG` const becomes
  `pub fn default_control() -> &'static str { "required" }`; a new
  `pub struct InstallOpts { service, control: String, module_args, ... }`
  threads the control flag to `build_line`.
- `~/sources/syauth/crates/syauth-cli/tests/snapshots/cli__install_pam_help_snapshot.snap`
  (modified) — new `--control` row.
- `~/sources/syauth/crates/syauth-cli/tests/install_pam.rs` (modified)
  — new `control_flag_round_trips_through_build_line` test.
- `src/syauth.rs::run` (modified) — `install-pam` + `uninstall-pam`
  dispatch.
- `src/syauth.rs::install_pam_args_builder` (new) — pure fn that
  builds the syauth-cli argv vector; tested without shelling out.

**Tests:**
- syauth repo: `control_flag_round_trips_through_build_line` plus
  the snapshot accept.
- `src/syauth.rs::tests::install_pam_args_builder_passes_sufficient_by_default`
  — pure-fn assertion on the argv vector, no sudo / no fs writes.
- `src/syauth.rs::tests::install_pam_args_builder_includes_timeout_8000`
  — argv contains `timeout=8000`, not `timeout=1200`.

**Definition of Done:**
- [ ] `syauth install-pam --service sudo --control sufficient
      --module-args timeout=8000 --yes` writes
      `auth sufficient pam_syauth.so timeout=8000` at the top of
      `/etc/pam.d/sudo` with the `.bak` snapshot intact.
- [ ] `sy syauth install-pam --service sudo` prints the same diff
      and exits 0 on a green host.
- [ ] `sy syauth uninstall-pam --service sudo` restores the `.bak`.
- [ ] After install: a single `sudo true` (no `-n`) succeeds with
      `grantors=pam_syauth` (verified by tailing the journal for
      `PAM:authentication grantors=pam_syauth`).
- [ ] `make lint && make test` green in both repos.

---

## Step 4 — Desktop notifications hook the pair + unlock state changes

**Goal:** the operator sees one notification per state transition,
not five. Today the pair flow only surfaces in the waybar pill;
unlock attempts only go to `/var/lib/syauth/last.log`.

**Audit-log parsing contract.** `/var/lib/syauth/last.log`
interleaves two formats (daemon CSV + PAM ISO line) — see
pre-flight. The notifier MUST filter on `NF == 6 && $4 ~ /^[0-9]+$/`
(or equivalent) when reading the file, mirroring the fix in
`~/sources/syauth/scripts/e2e-unlock.sh`. Without that filter the
ISO lines parse as `elapsed_ms = 0` and the notifier mis-classifies
every unlock as "instant transport-error".

**Files:**
- `src/syauth.rs::notify` (existing — already shells `notify-send`)
  — extend the call sites so a state change fires exactly one
  notification:
  - pair request appears → "syauth: pair request — code 000000,
    click bar to accept"
  - pair completes (bond saved) → "syauth: paired with fedora"
  - unlock succeeds → silent (one notification per `sudo` would
    be noisy; the bar pill's `active` class already covers the
    visual signal)
  - unlock denied → "syauth: unlock denied (peer revoked|no bond|
    auth-err|transport-error)"
- `src/syauth.rs::audit_log_tail` (new) — pure fn that takes the
  last N lines of `/var/lib/syauth/last.log`, filters non-CSV
  lines, and returns a typed `Vec<UnlockOutcome>`.
- `src/syauth.rs::notify_dispatcher` (new) — state machine that
  compares the most recent outcome against the last-notified
  outcome (cached at `~/.local/state/sy/syauth.last-outcome`)
  and fires `notify-send` exactly once per transition.

**Tests:**
- `src/syauth.rs::tests::audit_log_tail_skips_iso_lines` —
  fixture with mixed CSV + ISO lines, asserts only the CSV
  outcomes survive.
- `src/syauth.rs::tests::notify_is_idempotent_per_state` —
  feed the parser two identical outcome snapshots and assert
  `notify-send` is called exactly once.
- `src/syauth.rs::tests::notify_fires_on_pair_completion` —
  scripted transition from `pending` to `ok` fires one
  `notify-send` call.
- `src/syauth.rs::tests::notify_fires_on_unlock_denied` —
  outcome row with `reason=bad-signature` fires one
  `notify-send` call carrying the reason verbatim.

**Definition of Done:**
- [ ] State-transition notifier fires exactly once per transition,
      not per poll.
- [ ] No silent failures: every transition that doesn't notify
      writes a line to `~/.local/state/sy/syauth.log`.
- [ ] `audit_log_tail` rejects the ISO-timestamp format from
      pam_syauth.
- [ ] `make lint && make test` green.

---

## Step 5 — Operator setup doc + `sy syauth doctor`

**Goal:** a new operator can go from "I just installed sy" to
"my phone unlocks sudo" with one command and one doc page.

`syauth doctor --json` already exists upstream and probes the
exact chain we need (daemon liveness, bonds file, keys file modes,
BlueZ adapter, systemd user unit state, audit-log tail, XDG
runtime caveat). The `sy` side is a thin reformatter, not a
re-implementation.

**Files:**
- `README.md` (modified) — add a short "syauth" section pointing
  at the new doc.
- `docs/syauth-setup.md` (new) — five-step setup:
  1. Build / install `pam_syauth.so`:
     `cargo build --release -p syauth-pam` in `~/sources/syauth`,
     then `sudo install -m 644
     target/release/libpam_syauth.so
     /usr/lib64/security/pam_syauth.so`. The PAM module replace
     must use `install`, not `cp` — replacing the file in-place
     while a running `sudo` has it mmap'd segfaults the in-flight
     process. (`cp` writes byte-by-byte; `install` is a rename
     under the hood.)
  2. Install the daemon: `syauth install-presenced --live` (the
     subcommand exists upstream; it writes
     `~/.config/systemd/user/syauth-presenced.service`, reloads,
     enables, and starts it).
  3. `sy syauth install-pam --service sudo` (step 3 of this
     roadmap).
  4. Install the Android app on the phone (link to
     `syauth-android` APK build instructions).
  5. On desktop: `syauth pair --waybar --force` (or
     `syauth pair --yes` for a non-interactive run); on phone:
     tap Pair, pick this desktop, confirm the 6-digit
     LESC numeric-comparison code, confirm the 4-word OOB.
  6. Verify: `sudo true` should grant access via
     `grantors=pam_syauth` while the phone is in range; pulling
     the phone off the desk and re-running `sudo true` should
     fall through to FIDO / password.

  Add a troubleshooting section with the three concrete failure
  modes we hit during the 2026-05-19 e2e bring-up:
  - **`transport-error` with `t_start_ms == t_end_ms`** in
    `journalctl --user -u syauth-presenced` — the phone is not
    subscribed to the challenge characteristic. Toggle Bluetooth
    on the phone (`adb shell svc bluetooth disable && enable`)
    so the persistent GATT client reconnects to the daemon's
    current GATT app registration. Stale subscriptions from a
    previous daemon process survive the daemon restart on BlueZ
    and need an explicit phone-side reconnect to flush.
  - **`unlock denied reason=transport-error`** in
    `pam_syauth`'s journal but daemon shows `outcome=ok` —
    `DAEMON_RESPONSE_BUDGET` in the deployed `pam_syauth.so` is
    too small for real biometric reaction. Rebuild from
    `~/sources/syauth` HEAD (which carries the 8000 ms budget)
    and reinstall with `install -m 644`.
  - **`peer already bonded`** during pair — pass
    `--force` to overwrite the on-disk bond record (the OS-level
    LESC bond is untouched either way; `--force` only swaps the
    bond TOML row). The flag was added during DEV-001 closure.
- `src/syauth.rs::run` (modified) — add a `"doctor"` action that
  shells out to `syauth doctor --json` and reformats the typed
  output into the OK / WARN / FAIL one-line-per-probe surface.
  Each probe maps to one line:
  - daemon.state == "up" → OK; otherwise FAIL with hint
    "systemctl --user status syauth-presenced".
  - bonds_file.exists && bonds_file.parseable && count > 0 →
    OK; otherwise WARN with hint "syauth pair --waybar".
  - keys.files[].ok all true → OK; otherwise FAIL with hint
    "chmod 0600 /var/lib/syauth/keys/*.bin".
  - bluez_adapter == "ok" → OK; "unknown" → WARN; else FAIL.
  - systemctl == "active" → OK; otherwise FAIL with hint
    "systemctl --user enable --now syauth-presenced".
  - last_log_tail non-empty → OK; empty → WARN with hint
    "no unlock attempts yet — try sudo true".
  - Plus a `pam_so_present` check (the doctor JSON does not
    cover this — sy adds it locally):
    `Path::new("/usr/lib64/security/pam_syauth.so").exists()`.
  - Plus a `pam_so_wired` check:
    `grep -q pam_syauth.so /etc/pam.d/sudo`.

**Tests:**
- `src/syauth.rs::tests::doctor_aggregates_check_results` — feed
  a fixture `syauth doctor --json` blob and assert the aggregate
  exit code (0 if all OK, 1 if any FAIL, 2 if any WARN-only).
- `src/syauth.rs::tests::doctor_emits_one_line_per_check` —
  output format is greppable: `key=value status=ok|warn|fail`
  with the hint as a trailing `hint="..."` field.

**Definition of Done:**
- [ ] `sy syauth doctor` runs in under 2 seconds on a green host
      (the upstream `syauth doctor` already meets this; sy adds
      two cheap fs probes) and prints one OK/WARN/FAIL line per
      check.
- [ ] `docs/syauth-setup.md` walks a fresh-host operator through
      the six steps, with a troubleshooting section for the
      three concrete failures listed above.
- [ ] `make lint && make test` green.
- [ ] One real e2e run from a fresh-host snapshot end-to-end
      (login → install-pam → install-presenced → pair → sudo
      true grants with `grantors=pam_syauth`) is captured in the
      PR description as a recipe.

---

## Out of scope (deliberately)

- BlueZ adapter management UI — `bluetoothctl` already covers
  this; `sy syauth doctor` only reads adapter state (via
  `syauth doctor`'s probe), never writes.
- Multi-host bond management (the SPEC carries a hook but
  v0.1 is single-host; revisit when SPEC §3.x grows the
  multi-host clause).
- Stack-bar (`sy stack bar`) integration — the bar's slot model
  isn't tuned for transient pill states yet; revisit after the
  stack-bar-ux roadmap closes.
- Phone-app distribution — the Android APK lives in the syauth
  repo; sy is not the right place to host or sign it.
- SPEC §4.3 latency gate. The 2026-05-19 e2e bring-up measured
  p50 = 1588 ms, p99 = 3027 ms over five unlocks (one
  fingerprint tap per unlock). The SPEC §4.3 budget
  (p50 ≤ 1500 ms, p99 ≤ 2000 ms) was calibrated against an
  auto-approve path. Reconciling the budget with the real
  BiometricPrompt-in-critical-path reality is a JOURNEY-S-019
  closure-appendix item in the syauth repo, not work for `sy`.

## Cross-repo dependencies

Steps 1, 2, 4, 5 depend only on the `syauth` CLI shape committed
at the top of the syauth repo (`~/sources/syauth`), which already
includes:

- `syauth pair --force` (closed during DEV-001..DEV-003 march on
  2026-05-17).
- `syauth status --json` and `syauth doctor --json` (machine-
  readable surfaces for the bar + doctor shim).
- `syauth install-presenced` (used by step 5's setup doc; runs
  the systemd user-unit install + enable + start).
- Android-local bond schema v2 with `keystore_alias` +
  `phone_pubkey_hex` populated by `persistFull` (DEV-002 closure;
  consumed by the phone, not by `sy`).
- `syauth-bond.toml` written atomically by the Android app at
  `filesDir/syauth-bond.toml`.

Step 3 requires a one-line patch in
`syauth/crates/syauth-cli/src/install_pam.rs` (adding `--control`).
Land that patch in the syauth repo first; step 3 of this roadmap
consumes it.
