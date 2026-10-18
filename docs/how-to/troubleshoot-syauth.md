<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to troubleshoot syauth

## Goal

Diagnose and repair the three concrete syauth failure modes
observed during e2e bring-up so phone-as-key `sudo` returns to a
working state.

## Prerequisites

- You followed
  [the syauth setup tutorial](../tutorials/syauth-setup.md) at
  least once and the chain worked end-to-end before it broke.
- `sudo` privileges on the host.
- `adb` access to the paired Android phone (required for the
  first failure mode only).
- The [`syauth`](https://github.com/dmytrogajewski/syauth) repo
  cloned at `~/sources/syauth` (required for the second failure
  mode only).

## Steps

### Desktop apps skip phone and FIDO authentication

PolicyKit authenticates through `polkit-1`, while sudo uses `sudo`.
Install sy's managed configuration to reuse sudo's authentication chain:

```bash
make install-system-auth DRY_RUN=--dry-run
make install-system-auth
pkexec /usr/bin/true
```

The installer preserves an existing local override as
`/etc/pam.d/polkit-1.sy-backup`. Account and session checks retain Fedora's
`system-auth` configuration. Existing sudo phone/FIDO/password ordering is
used for each PolicyKit challenge; PolicyKit authorization rules still apply.

### Daemon runs but has no paired peers

Compare `syauth list` with `syauth status --json`. A stored revoked bond does
not count as an active peer, even if `syauth doctor` counts the record.
Complete a fresh pairing instead of changing a revoked record by hand.

If the phone reports `host-pubkey read failed status=1` after Bluetooth
pairing, update the companion app: the host key must be read before writing
the phone key, because that write can replace the pair GATT service.

Run `sy syauth doctor` first; it identifies most failures by
naming the failing probe and is enough to point you at one of the
three sections below:

```bash
sy syauth doctor
```

If the failure does not match one of the three patterns, file an
issue against [`syauth`](https://github.com/dmytrogajewski/syauth)
with the `doctor` output attached.

### Fix `transport-error` with `t_start_ms == t_end_ms`

Symptom: `journalctl --user -u syauth-presenced` shows challenges
that fail with `outcome=transport-error` and a `t_start_ms` equal
to `t_end_ms` (the challenge never reached the phone).

Cause: the phone has no usable challenge subscription. The desktop can retain
a notification writer after its connection closes; writing it fails and currently
rebuilds the GATT application. Android may also hold cached service handles from
an earlier registration. A phone-side reconnect can restore subscription.

1. Toggle Bluetooth on the phone over `adb`:

   ```bash
   adb shell svc bluetooth disable
   adb shell svc bluetooth enable
   ```

2. Re-run a sudo cycle and confirm the journal:

   ```bash
   sudo true
   journalctl _COMM=sudo --since '1 min ago' | grep grantors=pam_syauth
   ```

### Slow reconnection after Bluetooth is enabled or the phone returns

Keep the existing bond. Update the companion app to include the 2026-10-18
recovery repair: radio enable starts its owned client immediately, and setup
retries continue until notification subscription succeeds. Radio-off clients
pause retries. One nearby-phone cycle improved from about 22 seconds to 8
seconds; physical leave-and-return and Doze were not qualified by that sample.

Check `adb logcat -s syauth.bg.persistent` for `descriptor write status=0` and
the daemon journal for `phone subscribed`. A desktop `cached writer dead`
followed by GATT rebuilding identifies a separate desktop lifecycle problem.
The status field `last_connect_ms_ago` is currently stamped on challenges and
does not establish Bluetooth readiness. Repeating pairing is unnecessary for
ordinary range loss and does not fix stale notification sessions.

### Screen unlock stays silent but a later sudo prompts the phone

Swaylock authenticates after Enter; an empty password field can request phone
authentication. Check `/etc/pam.d/swaylock` for its phone entry or an included
authentication stack before adding one. The reference machine already has
phone, FIDO, and password authentication configured for swaylock.

Compare `journalctl _COMM=swaylock` with the daemon journal at the failed
attempt. A `transport-error` followed by `cached writer dead` and GATT rebuilding
means the request repaired transport and then fell through. A later sudo can
succeed after the phone resubscribes. Updating only PAM does not fix that cause.
The desktop transport repair tracks notification-session closure independently
and retains GATT registration during disconnects. See
[the screen-unlock investigation](../../specs/bugs/BUG-20261018-1736.md) for
verification and deployment status.

### Fix `unlock denied reason=transport-error` while the daemon shows `outcome=ok`

Symptom: `pam_syauth` denies the unlock with
`reason=transport-error`, but `journalctl --user -u syauth-presenced`
shows the matching challenge completed with `outcome=ok`.

Cause: the `DAEMON_RESPONSE_BUDGET` compiled into the deployed
`pam_syauth.so` is too small for a real BiometricPrompt reaction
(2–3 s). The PAM module gives up before the daemon's success
response arrives. Rebuild from `syauth` HEAD (which carries an
8000 ms budget) and reinstall:

1. Rebuild `pam_syauth` in release mode:

   ```bash
   cd ~/sources/syauth
   cargo build --release -p syauth-pam
   ```

2. Install the rebuilt module with `install`, not `cp`. `install`
   renames the file; `cp` overwrites the inode and segfaults any
   in-flight `sudo` that already has the old module mmap'd:

   ```bash
   sudo install -m 644 \
     target/release/libpam_syauth.so \
     /usr/lib64/security/pam_syauth.so
   ```

3. Re-run a sudo cycle and confirm the journal shows
   `grantors=pam_syauth`.

### Fix `peer already bonded` during `syauth pair`

Symptom: `syauth pair` exits with `peer already bonded` and
refuses to write a new bond record.

Cause: the on-disk bond record at `/var/lib/syauth/bonds.toml`
still carries a previous bond row. The OS-level LESC bond is
untouched; only the user-space bookkeeping is stale.

1. Re-run pair with `--force` to overwrite the on-disk row:

   ```bash
   syauth pair --waybar --force
   ```

2. Walk the phone-side pairing flow as described in
   [Step 5 of the tutorial](../tutorials/syauth-setup.md#step-5--pair-the-phone-with-the-desktop).

## Result

`sy syauth doctor` exits `0`, `sudo true` produces a
`grantors=pam_syauth` line in the journal with the phone in BLE
range, and a sudo cycle with the phone out of range falls through
to FIDO or password authentication without blocking.

## sy authorization dialog

The desktop shell provides `polkit-agent`, a session PolicyKit agent. It shows
PolicyKit's action and selected account, explains phone approval, displays
PAM's FIDO touch messages, and enables masked input when PAM asks for a password.
Cancel or close the window to cancel the request.

For a local build:

```bash
cargo build --release -p sy-desktop-shell
make install-polkit-agent DRY_RUN=--dry-run
make install-polkit-agent
systemctl --user daemon-reload
systemctl --user enable --now sy-polkit-agent.service
```

Exit the previous authentication agent before starting this service. Registration
fails if another agent already owns the session. Packaged desktop shell installs
the same unit using `/usr/libexec/sy/sy-desktop-shell`; Niri starts the service.
Preview without authenticating:

```bash
~/.local/libexec/sy/sy-desktop-shell polkit-agent --preview
```

Pairing requires the desktop's pair window as well as the phone picker. Run
`syauth pair --waybar --force` and then choose **Pair with computer** on the phone.
Compare and confirm the codes on both devices. The updated app refreshes its
paired home card and starts the unlock connection when **Done** is pressed.

The current sudo PAM configuration gives phone approval 8 seconds. A response
after that deadline cannot authorize the expired request; follow the desktop's
current FIDO/password prompt instead. Pairing remains valid after a timeout.
