# JOURNEY-20260725-2130: bring up a 4G/LTE USB modem declaratively via `sy wwan`

## Actor & Goal

- **Actor**: rice user on Fedora 43 + niri running sy. They plug a
  USB LTE stick (a "Vertell 4G" — internally a Fibocom L850 / Intel
  XMM7360 module presenting CDC-ACM + CDC-NCM ports) with a carrier
  SIM (MegaFon) into the laptop. ModemManager already probes it and
  NetworkManager exposes a `gsm` device, but the device sits
  `unavailable` because no mobile-broadband **connection profile**
  exists — nothing tells the modem which APN to register with.
  Secondary actor: an MCP/CLI agent driving the same surface.
- **Goal**: declare the modem's APN profile once, under sy's config
  tree, then `sy wwan enable` to materialise a NetworkManager `gsm`
  connection (APN `internet`, autoconnect on) so the modem registers
  on the home network and provides data whenever wi-fi is absent — and
  so a fresh machine + `sy apply` + `sy wwan enable` reproduces working
  4G with zero hand-edited NetworkManager files.
- **Hardest constraint**: **no snowflakes.** The working config the
  user reached by a manual `mmcli --simple-connect` must live in the
  repo (`configs/sy/wwan.toml`) and be reconciled idempotently — never
  a one-off `nmcli connection add` typed at a shell.

## Happy Path

1. **Declare.** `configs/sy/wwan.toml` carries one `[[profile]]`
   (name, apn, autoconnect, roaming). `sy apply` renders it to
   `~/.config/sy/wwan.toml` like every other sy config file.
2. **Reconcile.** `sy wwan enable` reads the config, and for each
   profile ensures a NetworkManager `gsm` connection exists and
   matches (create via `nmcli connection add` when absent, else
   `nmcli connection modify` — diff-driven, idempotent). Autoconnect
   is set so NM brings the bearer up automatically.
3. **Register.** ModemManager drives the modem to `registered/home`;
   the bearer attaches and routes data. `sy wwan status` (and
   `--json`) reports modem state, operator, signal, and whether the
   managed connection is present + active.
4. **Revert.** `sy wwan disable` deletes the managed connection
   (leaves the modem hardware untouched), restoring the pre-sy state.

## Acceptance

- `sy wwan enable` is idempotent: a second run reports "already
  matches" and makes no `nmcli modify` call.
- The connection name is deterministic and prefixed (`sy-wwan-*`) so
  reconcile can find and own exactly the profiles it manages without
  clobbering user-created connections.
- `sy wwan status --json` emits a stable, documented shape.
- Unit tests cover config parsing and the `nmcli` argv builder for
  both the add and modify paths (no live modem required).

## Root cause & resolution — MBIM mode switch (`sy wwan modeswitch`)

**Symptom.** The "Vertell 4G" stick is a Fibocom L850-GL (Intel
XMM7360). Out of the box it enumerated as USB id `8087:095a` —
`MODEM + 2×CDC-ACM + 3×CDC-NCM`, NCM mode, no MBIM/QMI. On Fedora 43
with ModemManager 1.24.2 it was claimed by the **`generic`** plugin
(AT-probe parsing flaky — model reported as the garbled
`L850 LTE Module","L850`), which drove it as **legacy AT+PPP dialup**:
the bearer came up `IPv4 method: ppp`, PPP took over the `ttyACM0`
control port, MM's follow-up AT commands timed out ~10×
(`port ttyACM0 timed out … marking modem as invalid`,
`No AT port available to run command`), and the modem dropped and
re-enumerated. Data flowed for ~25 s, then churned — enough to prove
the SIM/APN, not a usable link. There is **no FCC-unlock script** for
Intel `8087`, so the usual Fedora FCC-unlock fix did not apply.

**Fix (applied & verified).** Switching the modem to **MBIM** mode
resolves it completely:

```
AT+GTUSBMODE=7    # MBIM composition (was mode 0 = NCM+PPP)
AT+CFUN=15        # reset; modem re-enumerates
```

After the switch the modem comes up as USB id **`2cb7:0007`
(Fibocom L850-GL)** exposing `cdc_mbim` / `cdc-wdm0`; ModemManager
selects the **`fibocom`** plugin with `cdc-wdm0` as the primary port,
the bearer becomes **`method: static`** (IP/gateway/DNS from the
network), and the link is stable — verified continuously connected
well past the old 25 s churn point with live HTTP transit
(`http://cp.cloudflare.com/` → 307). ICMP to the gateway is dropped by
MegaFon, so `ping` is a false negative; use HTTP/TCP to test. The
switch **persists across power cycles** — a one-time operation per
modem.

**Firmware caveat.** The `xmm7360-usb-modeswitch` project warns that
mode-7 can trigger a reboot loop on some firmware (≥
`18500.5001.00.02.24.09`). This unit is `18500.5001.00.05.27.30` and
switched cleanly with **no** reboot loop (10/10 post-reset enumeration
samples stable). Because the risk is real on other firmware, the switch
is gated.

**Productised.** `sy wwan modeswitch` performs this reproducibly
instead of a hand-typed AT dance (no snowflakes):

- `sy wwan modeswitch` — read-only preview: stops MM, prints current
  GTUSBMODE + firmware, restarts MM. Non-destructive.
- `sy wwan modeswitch --yes` — stops MM, runs `AT+GTUSBMODE=7` +
  reset (`scripts/wwan_modeswitch.py`, stdlib-only termios), waits for
  re-enumeration, restarts MM, and confirms the `fibocom`/MBIM
  identity. `--revert` in the script returns to mode 0.

With MBIM in place `configs/sy/wwan.toml` ships `autoconnect = true`.
On a fresh machine the modem retains mode 7; only a factory-reset or
replacement unit needs `sy wwan modeswitch --yes` again.
