<!-- Template source: Good Docs Project reference template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/reference. Diátaxis quadrant: reference. -->

# `pam_syauth.so` reference

PAM module that authenticates the calling user against a paired,
LESC-bonded Android device over Bluetooth Low Energy.

## Synopsis

```
auth <control> pam_syauth.so [timeout=<ms>]
```

Installed at `/usr/lib64/security/pam_syauth.so`.

Wired into a PAM service file (typically `/etc/pam.d/sudo`)
through the `sy` wrapper:

```bash
sy syauth install-pam --service <service> --yes
```

## Description

`pam_syauth.so` is loaded by the PAM `auth` stack. On invocation
it asks the user-level `syauth-presenced.service` daemon to
challenge the paired phone over BLE. The daemon returns the
challenge outcome inside the configured `timeout`; the module
maps the outcome to a standard PAM result.

The module owns no state on disk. The pairing record lives at
`/var/lib/syauth/bonds.toml`, the daemon socket lives under the
user runtime directory, and the audit log is written to the
journal by `pam_syauth` and `syauth-presenced` independently.

The `sy syauth install-pam` wrapper bakes in the reality-corrected
defaults below. Passing `--control` or `--module-args` to the
wrapper overrides them per invocation.

## Control flags

| Flag | Default in sy wrapper | Behaviour |
|---|---|---|
| `sufficient` | yes | On success, PAM short-circuits the rest of the `auth` stack and returns success. On failure, PAM falls through to the next module in the stack (typical fall-through targets: FIDO, password). |
| `required` | no | On failure, PAM marks the overall stack as failed but continues evaluating subsequent modules. Use only when syauth must be present for any successful authentication. |
| `requisite` | no | On failure, PAM stops the stack immediately and returns failure. Hard requirement on syauth — locks the user out when the phone is unreachable. |
| `optional` | no | The module's result is ignored unless it is the only module in the stack. |

The `sy syauth install-pam` wrapper writes `sufficient` so a
syauth denial never blocks the remaining authenticators.

## Module arguments

| Argument | Type | Default in sy wrapper | Description |
|---|---|---|---|
| `timeout` | integer (milliseconds) | `8000` | Upper bound on how long `pam_syauth.so` waits for the daemon to return a challenge outcome before returning `PAM_AUTHINFO_UNAVAIL`. The measured BiometricPrompt reaction window on the reference Galaxy S25 Ultra hardware is 2–3 s. Values below `2000` time out under normal operation. |

## Exit conditions

| PAM result | Trigger |
|---|---|
| `PAM_SUCCESS` | The daemon returned `outcome=ok` within `timeout`. |
| `PAM_AUTH_ERR` | The daemon returned `outcome=denied` (user dismissed the BiometricPrompt or the bond does not match). |
| `PAM_AUTHINFO_UNAVAIL` | The daemon is not running, the socket is unreachable, the phone did not respond within `timeout`, or the bond record at `/var/lib/syauth/bonds.toml` is missing. |
| `PAM_USER_UNKNOWN` | The calling user has no bond record. |

Under the `sufficient` control flag, `PAM_AUTHINFO_UNAVAIL` and
`PAM_AUTH_ERR` cause PAM to fall through to the next module in
the stack.

## Files

| Path | Owner | Purpose |
|---|---|---|
| `/usr/lib64/security/pam_syauth.so` | `root:root`, mode `0644` | The PAM module itself. |
| `/etc/pam.d/<service>` | `root:root`, mode `0644` | The PAM service file the module is wired into. |
| `/etc/pam.d/<service>.bak` | `root:root`, mode `0644` | Pre-install snapshot written by `syauth install-pam`. Restored by `sy syauth uninstall-pam --service <service> --yes`. |
| `/var/lib/syauth/bonds.toml` | `root:root`, mode `0600` | LESC bond records, one row per paired phone. |
| `~/.config/systemd/user/syauth-presenced.service` | the user | The user-level daemon unit `syauth install-presenced` writes. |

## Examples

Install with the sy-wrapper defaults (`sufficient`, `timeout=8000`):

```bash
sy syauth install-pam --service sudo --yes
```

Install with an explicit control flag and a longer timeout:

```bash
syauth install-pam \
  --service sudo \
  --control sufficient \
  --module-args timeout=10000 \
  --yes
```

Inspect the resulting `/etc/pam.d/sudo` entry:

```bash
grep pam_syauth /etc/pam.d/sudo
```

Expected line (one of):

```
auth    sufficient    pam_syauth.so timeout=8000
```

Restore the pre-install snapshot:

```bash
sy syauth uninstall-pam --service sudo --yes
```

## See also

- [Tutorial: unlock sudo with your phone using syauth](../tutorials/syauth-setup.md)
- [How to troubleshoot syauth](../how-to/troubleshoot-syauth.md)
- [`pam.conf(5)`](https://man7.org/linux/man-pages/man5/pam.conf.5.html) for the upstream PAM control-flag and module-argument grammar.
