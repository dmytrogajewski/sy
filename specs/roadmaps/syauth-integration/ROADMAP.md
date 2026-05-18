# ROADMAP: syauth integration — operator-facing surface in `sy`

Source: `~/sources/syauth/specs/syauth/SPEC.md` (canonical syauth spec) and `~/sources/syauth/docs/known-gaps.md` (closed deviation rows DEV-001..DEV-004).

## Overview

syauth is a phone-as-key Linux PAM unlock module. The desktop ships
two binaries (`syauth` CLI + `pam_syauth.so`) and the phone ships an
Android app that holds the LESC-bonded Ed25519 keypair inside the
Keystore (DEV-002 closure). syauth on its own is a CLI + a PAM
shared object: the operator-facing surfaces (status pill in the bar,
LESC numeric-comparison popup, accept/reject buttons, install
helpers) belong in `sy`. This roadmap lands them in five ordered,
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
file. Steps 1–4 grow that surface; step 5 documents the operator
setup flow.

Pre-flight check before step 1: `~/sources/syauth/target/debug/syauth
--help` must list `pair / list / revoke / status / install-pam /
uninstall-pam`. The `pair` subcommand must accept `--force` (added
during DEV-001..DEV-003 closure) and write bonds in schema v2
(`keystore_alias` + `phone_pubkey_hex` populated by `persistFull`).

---

## Step 1 — `sy syauth status` pulls live bond + adapter state from the syauth CLI

**Goal:** a single command renders the operator-visible source of
truth: is a phone paired, is the adapter up, did the last unlock
succeed or fail. Today the waybar applet only renders the pair
request; in idle state the bar pill says nothing about the bond.

**Files:**
- `src/syauth.rs::run` (modified) — add a `"status"` action that
  shells out to `syauth status` and `syauth list`, parses the
  output, and prints a one-line summary (`bonded fedora · adapter
  hci0 ok · last unlock 2026-05-17T22:30:00Z ok`). Mirrors the
  format the bar pill needs in step 2.
- `src/syauth.rs::waybar_out` (modified) — when no pair request is
  pending, fall back to the same status string so the bar always
  has something to render instead of going blank.

**Tests:**
- New `src/syauth.rs::tests::status_renders_bonded_line` — feed a
  scripted `syauth list` output through a parser fn and assert the
  one-liner ends with `bonded fedora`.
- `src/syauth.rs::tests::status_handles_empty_bond_list` — empty
  `syauth list` output renders `not paired`.

**Definition of Done:**
- [ ] `sy syauth status` returns 0 when bonded, 1 when not, with the
      single-line output documented above.
- [ ] Waybar idle pill renders the status line instead of empty
      JSON.
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

**Files:**
- `src/syauth.rs::waybar_out` (modified) — branch on pair-request
  presence first (existing path); otherwise call into the step-1
  status parser and render the table above.
- `configs/waybar/style.css` (modified) — add `.custom-sy-syauth.ok`
  / `.degraded` / `.active` / `.unpaired` style hooks. Existing
  `.pending` stays.
- `configs/waybar/config.jsonc:84` (modified) — bump
  `custom/sy-syauth` poll interval back to 5s when idle (current 1s
  is only needed during pair); the waybar `signal` field stays so
  the pair flow can wake the bar on-demand.

**Tests:**
- `src/syauth.rs::tests::waybar_renders_pending_when_request_present`
  — existing test, keep.
- `src/syauth.rs::tests::waybar_renders_ok_class_when_bonded_idle`
  — new.
- `src/syauth.rs::tests::waybar_renders_unpaired_when_no_bond`
  — new.

**Definition of Done:**
- [ ] All five pill states render correctly under a Robolectric-
      style fake (no live BlueZ adapter required in tests).
- [ ] Style hooks documented in `configs/waybar/style.css` near
      `custom-sy-syauth`.
- [ ] `make lint && make test` green.

---

## Step 3 — `sy syauth install-pam <service>` wraps the underlying CLI with the right control flag

**Goal:** the operator runs ONE command to make syauth the primary
auth on a PAM service. The syauth CLI's `install-pam` currently
hard-codes `auth required pam_syauth.so` — that's wrong for the
"only when device connected" semantic the operator wants (a
required-failing syauth blocks the stack even when the phone is in
range but the user wants fingerprint instead). This step:

1. Upstream: patch `~/sources/syauth/crates/syauth-cli/src/install_pam.rs`
   to accept a `--control <flag>` argument, default still
   `required` for backward compatibility, accepts `sufficient` and
   the bracketed forms (`[success=done auth_err=die default=ignore]`).
   New `pair_help_snapshot.snap` + new unit test that pins the flag.
2. `src/syauth.rs::run` — add an `"install-pam"` action that takes a
   `--service` and `--control` (defaults to `sufficient`), shells
   out to `syauth install-pam --service X --control sufficient
   --module-args timeout=1200 --yes`, prints the resulting file
   diff to stdout, and reminds the operator to verify with
   `pamtester <service> $USER authenticate`.
3. `src/syauth.rs::run` — companion `"uninstall-pam"` action that
   restores from the `.bak` snapshot the CLI wrote.

**Files:**
- `~/sources/syauth/crates/syauth-cli/src/install_pam.rs` (modified
  in the syauth repo, cross-repo dependency — call out in the commit
  message).
- `~/sources/syauth/crates/syauth-cli/tests/snapshots/cli__install_pam_help_snapshot.snap`
  (modified) — new `--control` row.
- `src/syauth.rs::run` (modified) — `install-pam` + `uninstall-pam`
  dispatch.
- `src/syauth.rs::install_pam_args_builder` (new) — pure fn that
  builds the syauth-cli invocation; tested without shelling out.
- `configs/waybar/config.jsonc` — no change (waybar doesn't drive
  PAM install).

**Tests:**
- `~/sources/syauth/crates/syauth-cli/src/install_pam.rs` — new
  `control_flag_round_trips_through_build_line` + new
  `install_pam_help_snapshot.snap` accept.
- `src/syauth.rs::tests::install_pam_args_builder_passes_sufficient_by_default`
  — pure-fn assertion on the argv vector, no sudo / no fs writes.

**Definition of Done:**
- [ ] `syauth install-pam --service sudo --control sufficient --yes`
      writes `auth sufficient pam_syauth.so timeout=1200` at the
      top of `/etc/pam.d/sudo` with the `.bak` snapshot intact.
- [ ] `sy syauth install-pam --service sudo` prints the same diff
      and exits 0 on a green host.
- [ ] `sy syauth uninstall-pam --service sudo` restores the `.bak`.
- [ ] `make lint && make test` green in both repos.

---

## Step 4 — Desktop notifications hook the pair + unlock state changes

**Goal:** the operator sees one notification per state transition,
not five. Today the pair flow only surfaces in the waybar pill;
unlock attempts only go to `/var/lib/syauth/last.log`.

**Files:**
- `src/syauth.rs::notify` (existing — already shells `notify-send`)
  — extend the call sites so a state change fires exactly one
  notification:
  - pair request appears → "syauth: pair request — code 000000,
    click bar to accept"
  - pair completes (bond saved) → "syauth: paired with fedora"
  - unlock succeeds → silent (last-log line is enough; an unlock per
    sudo would be noisy)
  - unlock denied → "syauth: unlock denied (peer revoked|no bond|
    auth-err)"
- `src/syauth.rs::tests::notify_is_idempotent_per_state` (new) —
  feed the parser two identical status snapshots and assert
  `notify-send` is called exactly once.

**Tests:**
- the new idempotency test above.
- `src/syauth.rs::tests::notify_fires_on_pair_completion` (new) —
  scripted transition from `pending` to `ok` fires one
  `notify-send` call.

**Definition of Done:**
- [ ] State-transition notifier fires exactly once per transition,
      not per poll.
- [ ] No silent failures: every transition that doesn't notify
      writes a line to `~/.local/state/sy/syauth.log`.
- [ ] `make lint && make test` green.

---

## Step 5 — Operator setup doc + `sy syauth doctor`

**Goal:** a new operator can go from "I just installed sy" to
"my phone unlocks sudo" with one command and one doc page.

**Files:**
- `README.md` (modified) — add a short "syauth" section pointing at
  the new doc.
- `docs/syauth-setup.md` (new) — five-step setup:
  1. Build / install `pam_syauth.so` (`make install-syauth` or copy
     from `~/sources/syauth/target/release/`).
  2. `sy syauth install-pam --service sudo`.
  3. Install the Android app on the phone (link to `syauth-android`
     APK).
  4. On desktop: `syauth pair --yes` (or via the waybar applet);
     on phone: tap Pair, pick this desktop, confirm the 6-digit
     code, confirm the 4-word OOB.
  5. Verify: `sudo whoami` should grant access while the phone is
     in range; pulling the phone off the desk and re-running
     `sudo whoami` should fall through to fingerprint / password.
- `src/syauth.rs::run` (modified) — add a `"doctor"` action that
  runs all of:
  - `syauth status` — adapter + bond state
  - `test -e /usr/lib64/security/pam_syauth.so`
  - `grep -q pam_syauth.so /etc/pam.d/sudo`
  - `adb devices` (if `adb` is on PATH) — phone reachable
  - `bluetoothctl info <peer-mac>` — OS-level LE bond present
  Each check prints OK / WARN / FAIL with a one-line remediation
  hint pointing at the relevant setup-doc anchor.

**Tests:**
- `src/syauth.rs::tests::doctor_aggregates_check_results` — feed
  scripted check outputs and assert the aggregate exit code (0 if
  all OK, 1 if any WARN or FAIL).
- `src/syauth.rs::tests::doctor_emits_one_line_per_check` — output
  format is greppable.

**Definition of Done:**
- [ ] `sy syauth doctor` runs in under 5 seconds on a green host
      and prints one OK/WARN/FAIL line per check.
- [ ] `docs/syauth-setup.md` walks a fresh-host operator through
      the five steps, with a fallback section for "syauth refuses
      to unlock — what now".
- [ ] `make lint && make test` green.
- [ ] One real e2e run from a fresh-host snapshot end-to-end (login
      → install-pam → pair → sudo whoami) is captured in the PR
      description as a recipe.

---

## Out of scope (deliberately)

- BlueZ adapter management UI — `bluetoothctl` already covers this;
  `sy syauth doctor` only reads adapter state, never writes.
- Multi-host bond management (the SPEC carries a hook but
  v0.1 is single-host; revisit when SPEC §3.x grows the
  multi-host clause).
- Stack-bar (`sy stack bar`) integration — the bar's slot model
  isn't tuned for transient pill states yet; revisit after the
  stack-bar-ux roadmap closes.
- Phone-app distribution — the Android APK lives in the syauth
  repo; sy is not the right place to host or sign it.

## Cross-repo dependencies

Steps 1, 2, 4, 5 depend only on the `syauth` CLI shape committed at
the top of the syauth repo (`~/sources/syauth`), which already
includes:

- `syauth pair --force` (closed during DEV-001..DEV-003 march on
  2026-05-17).
- `BondRecord` schema v2 with `keystore_alias` + `phone_pubkey_hex`
  populated by `persistFull` (DEV-002 closure).
- `syauth-bond.toml` written atomically by the Android app at
  `filesDir/syauth-bond.toml`.

Step 3 requires a one-line patch in
`syauth/crates/syauth-cli/src/install_pam.rs` (adding `--control`).
Land that patch in the syauth repo first; step 3 of this roadmap
consumes it.
