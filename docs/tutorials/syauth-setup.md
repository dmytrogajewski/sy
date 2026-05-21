<!-- Template source: Good Docs Project tutorial template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/tutorial. Diátaxis quadrant: tutorial. -->

# Tutorial: unlock sudo with your phone using syauth

## Introduction

In this tutorial you wire up [syauth](https://github.com/dmytrogajewski/syauth)
— the phone-as-key PAM module `sy` wraps — on a fresh `sy` host so
that `sudo` consults your phone over Bluetooth Low Energy before
falling back to the standard authentication stack. You build and
install `pam_syauth.so`, bring up the user-level daemon, edit
`/etc/pam.d/sudo` through the `sy` wrapper, sideload the companion
Android app, pair the phone with the desktop, and confirm the
end-to-end unlock chain.

When you finish, `sudo true` on your laptop succeeds whenever your
phone is in BLE range with the screen unlocked, the audit log
shows a `grantors=pam_syauth` line, and pulling the phone off the
desk transparently falls through to FIDO or password authentication.

The PAM `sufficient` semantic means syauth wins the stack when it
returns success, but a denial never blocks the remaining
authenticators. The `timeout=8000` module argument matches the
real BiometricPrompt reaction window (2–3 s) measured on the
reference Galaxy S25 Ultra hardware.

## Prerequisites

- A working `sy` install — complete
  [the bring-up tutorial](getting-started.md) first so
  `sy --help` works and `sy.target` is active.
- The [`syauth`](https://github.com/dmytrogajewski/syauth) repo
  cloned at `~/sources/syauth`. The Android sources live under
  `~/sources/syauth/syauth-android`.
- An Android phone (Android 13 or later) with developer mode and
  `adb` access enabled.
- The desktop and the phone in BLE range of each other.
- `sudo` privileges on the host.
- `pamtester` installed for the post-step verification:

  ```bash
  sudo dnf install -y pamtester
  ```

## Step 1 — Build and install `pam_syauth.so`

Build the PAM module in release mode and copy it into the system
PAM library directory. Use `install`, not `cp`: `install` performs
a rename under the hood, so an in-flight `sudo` that has the old
inode mmap'd keeps reading the old bytes until it exits. `cp`
overwrites the existing inode byte by byte and segfaults that
in-flight reader.

```bash
cd ~/sources/syauth
cargo build --release -p syauth-pam
sudo install -m 644 \
  target/release/libpam_syauth.so \
  /usr/lib64/security/pam_syauth.so
```

## Step 2 — Install the syauth user daemon

`syauth install-presenced --live` writes
`~/.config/systemd/user/syauth-presenced.service`, runs
`systemctl --user daemon-reload`, enables the unit, and starts it.
The `--live` flag runs the daemon foreground for a quick smoke
test before handing control to systemd:

```bash
syauth install-presenced --live
```

Confirm the unit is healthy:

```bash
systemctl --user status syauth-presenced
journalctl --user -u syauth-presenced -n 20
```

## Step 3 — Wire `pam_syauth.so` into the sudo PAM stack

`sy syauth install-pam` is a thin reformatter over the upstream
`syauth install-pam` that bakes in the reality-corrected defaults
(`--control sufficient`, `--module-args timeout=8000`). It writes
a `.bak` snapshot of `/etc/pam.d/sudo` before editing, so the
change is reversible with `sy syauth uninstall-pam --service sudo --yes`:

```bash
sy syauth install-pam --service sudo --yes
```

Verify the inserted line without spending a `sudo` cycle:

```bash
pamtester sudo $USER authenticate
```

## Step 4 — Install the syauth Android app

Build and sideload the companion app from the upstream sources.
The app holds the LESC-bonded Ed25519 keypair in the Android
Keystore:

```bash
cd ~/sources/syauth/syauth-android
# Follow the build steps in this directory's README.md, then:
adb install -r app/build/outputs/apk/release/app-release.apk
```

Open the app once after install so it requests the Bluetooth and
notification permissions it needs to respond to challenges.

## Step 5 — Pair the phone with the desktop

Run the pairing flow on the desktop. The `--waybar` flag routes
the 6-digit numeric-comparison code through the waybar pill so you
can read it without opening a terminal window:

```bash
syauth pair --waybar --force
```

On the phone:

1. Open the syauth app.
2. Tap **Pair**.
3. Pick this desktop from the BLE scan list.
4. Confirm the 6-digit LESC numeric-comparison code matches the
   one shown in the waybar pill.
5. Confirm the 4-word out-of-band phrase.

## Step 6 — Verify the unlock chain

With the phone on the desk and the screen unlocked, trigger a
sudo cycle and confirm syauth grants it:

```bash
sudo true
journalctl _COMM=sudo --since '1 min ago' | grep grantors=pam_syauth
```

The `grep` prints one `grantors=pam_syauth` line for the cycle
you just ran.

## Verify

Confirm three things, in order:

1. The PAM module is on disk and wired into `sudo`:

   ```bash
   sy syauth doctor
   ```

   The command runs every probe in the chain (daemon liveness,
   bonds file, key file modes, BlueZ adapter, systemd user unit
   state, audit-log tail, plus the two sy-only `pam_so_present`
   and `pam_so_wired` probes) and exits `0` when all checks pass.

2. A live sudo cycle invokes syauth:

   ```bash
   sudo true
   journalctl _COMM=sudo --since '1 min ago' | grep grantors=pam_syauth
   ```

   You see one `grantors=pam_syauth` line.

3. Pulling the phone off the desk does not lock you out:

   Walk the phone out of BLE range (or disable Bluetooth on the
   phone) and run:

   ```bash
   sudo true
   ```

   The cycle succeeds via the next authenticator in the stack
   (FIDO or password). The `sufficient` control flag means a
   syauth denial never blocks the remaining authenticators.

When all three checks pass, phone-as-key sudo is live on your
host.

## Next steps

- If a step in this tutorial does not behave the way it is
  written here, see
  [how to troubleshoot syauth](../how-to/troubleshoot-syauth.md).
- For the full set of PAM control flags and module arguments
  `pam_syauth.so` accepts, see
  [the syauth PAM module reference](../reference/syauth-pam-module.md).
