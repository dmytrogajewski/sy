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

Cause: the phone is not subscribed to the challenge
characteristic. Stale subscriptions from a previous daemon process
survive a daemon restart on BlueZ and need an explicit phone-side
reconnect to flush.

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
