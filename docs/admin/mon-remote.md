# Reading `sy mon` from a remote host

`sy` is single-host by design — the `sy-mon-collect` aggregator binds
a Unix-domain socket under `$XDG_RUNTIME_DIR/sy/mon.sock` and never
listens on TCP. Per [SPEC §3 anti-goals][spec-antigoals], a network
exposition surface would drag in trust-boundary work, secret
management, and firewall rules — explicit snowflake hazard.

If you genuinely want to scrape `sy mon` from another machine, bridge
the UDS to TCP yourself with `socat` so the trust boundary stays
outside sy.

## Recipe — `socat` UDS-to-TCP bridge

On the **sy host** (the machine running `sy-mon-collect`), expose the
aggregator's socket on a local TCP port. Replace `9091` with any free
port; bind to `127.0.0.1` so it does not face the network until you
deliberately tunnel through `ssh`:

```sh
socat \
  TCP-LISTEN:9091,bind=127.0.0.1,reuseaddr,fork \
  UNIX-CONNECT:$XDG_RUNTIME_DIR/sy/mon.sock
```

On the **remote host** (your laptop), tunnel the local port through
`ssh` and run a single-shot snapshot request the same way `sy mon
snapshot --json` would — but against the TCP-bridged socket:

```sh
ssh -L 9091:127.0.0.1:9091 user@sy-host -N &
socat - TCP:127.0.0.1:9091 < snapshot-request.json
```

`snapshot-request.json` is the JSON-RPC frame that `system.mon.snapshot`
expects; see [`docs/agents/mon-schema.md`](../agents/mon-schema.md)
for the MCP tool surface and the wire shape of the reply.

## Trust boundary notes

- **No authentication on the TCP side.** `socat TCP-LISTEN` is
  unauthenticated. Always bind to `127.0.0.1` and tunnel through
  `ssh`; never bind to `0.0.0.0` on an untrusted network.
- **Read-only by design.** The `sy mon` plane is read-only. `sy mon`
  exposes no mutating IPC ops, so even an unauthorised reader of the
  bridged port cannot change state. State changes go through
  `sy aiplane`, `sy agt`, `sy knowledge`, etc., each with their own
  `--yes` / `--dry-run` ergonomics.
- **No remote scrape promise.** This recipe is a workaround, not a
  contract. The UDS path is the supported interface; any wire change
  lands behind a `schema_version` bump per
  [`docs/agents/mon-schema.md`](../agents/mon-schema.md).

## Why not built-in?

> "If a user wants remote scraping, they bridge the UDS to TCP with
> `socat` themselves … which keeps the trust boundary outside sy."
>
> — `specs/research/sy-mon/SPEC.md` §3 anti-goals

A TCP listener inside `sy` would force every install to ship firewall
rules, certificate plumbing, and an authentication story. The single
documented `socat` recipe above is the lighter, lower-blast-radius
alternative for the handful of operators who actually need it.

[spec-antigoals]: ../../specs/research/sy-mon/SPEC.md
