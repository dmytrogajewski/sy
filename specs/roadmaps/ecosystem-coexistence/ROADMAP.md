# ROADMAP: sy — ecosystem coexistence (coordination hub)

> Source spec: [SPEC.md](./SPEC.md) (canonical zero-config coexistence contract)
> sy's role: host the ecosystem registry, supervise sibling daemons, surface
> coexistence state, and detect cross-component conflicts.
> Each item is one independently-testable journey. IDs are stable; insert with
> `ECO-SY-0XX` between existing IDs, never renumber.

---

### Step 1: Ecosystem registry IPC + descriptor store

**Description:** Add an `ecosystem` surface to the `sy` IPC v1 plane:
`ecosystem.register`, `ecosystem.update`, `ecosystem.list`, plus a watched
fallback directory `$XDG_RUNTIME_DIR/sy/ecosystem/<component>.json` for
components that start before `sy`. Descriptors follow SPEC §3.

**DoR:** SPEC §3 reviewed.

**DoD:**
- [ ] `ecosystem.register`/`update`/`list` ops accepted over IPC v1 with the SPEC §3 schema.
- [ ] A component registering, then a second `list`, returns the descriptor verbatim.
- [ ] Descriptor files dropped in the fallback dir before `sy` starts are ingested on start.
- [ ] Stale descriptors (component gone) are pruned on a liveness check.
- [ ] Unit + integration tests cover register → list, file-fallback ingest, and pruning.

**Risks:** schema drift across components — mitigate by versioning the descriptor (`schema_version`) and intersecting on read.

---

### Step 2: `sy ecosystem` CLI + MCP tool

**Description:** Expose the registry read-only via `sy ecosystem [--json]` and an
`ecosystem_list` MCP tool, so a human, an agent, or cathouse can read who owns
what and the live flags.

**DoR:** Step 1 done.

**DoD:**
- [ ] `sy ecosystem` prints the components, roles, owned resources, and flags.
- [ ] `sy ecosystem --json` emits the structured set (stable schema).
- [ ] `ecosystem_list` MCP tool returns the same payload over the line-delimited JSON-RPC surface.
- [ ] Read-only: no mutating op is exposed.
- [ ] Tests assert the human and `--json` shapes and the MCP envelope.

---

### Step 3: Supervise cathouse-desktopd under sy.target

**Description:** Add `cathouse-desktopd.service` to the `sy.target` group with the
same `Type=notify` / `BindsTo=` / `WatchdogSec=` conventions as the other planes,
auto-enabled when cathouse is installed and detected.

**DoR:** cathouse ships a `sy`-compatible unit.

**DoD:**
- [ ] `sy.target` brings up `cathouse-desktopd` alongside `syauth-presenced` when present.
- [ ] Absence of cathouse does not fault `sy.target` (optional dependency).
- [ ] `sy apply` wiring is idempotent and reproducible (no snowflake).
- [ ] Test: a fixture with cathouse installed brings the unit up; a fixture without it does not fault the target.

**Risks:** ordering vs the registry — mitigate by making registration lazy/retried so daemon start order does not matter.

---

### Step 4: stack-bar pills per ecosystem component

**Description:** Render a bar pill for each registered component (prrr link
state, syauth unlock/presence, cathouse companion link) from the registry, so
coexistence state is visible at a glance.

**DoR:** Step 2 done.

**DoD:**
- [ ] Pills appear for each registered component and update on flag change.
- [ ] A missing/absent component shows no pill (no error tile).
- [ ] Pills derive purely from the registry (no per-component bespoke probe).
- [ ] Test: registry fixture renders the expected pill set.

---

### Step 5: `sy doctor` cross-component conflict checks

**Description:** Add coexistence checks to `sy doctor`: a full-tunnel VPN without
local-LAN exclusion while a companion is present; two components claiming the
same owned resource; a sibling referencing an unknown `peer_id`; a daemon not
registered.

**DoR:** Steps 1–2 done.

**DoD:**
- [ ] `sy doctor` reports a clear, actionable finding for each conflict class above.
- [ ] Each finding names the offending component and the remediation.
- [ ] Clean ecosystem yields a green result.
- [ ] Tests drive each conflict fixture to its finding and the clean case to green.

**Risks:** false positives annoy users — mitigate by checking the registry flags (e.g. prrr `excluded_cidrs`) rather than guessing.

---

### Step 6: Relay an authorized "enable prrr sharing" request

**Description:** Per ecosystem decision §9.4, prrr client-to-client sharing is
drivable from the ecosystem, not only prrr's local toggle. Add an
`ecosystem.request-sharing` op (and MCP tool) that forwards a caller's authorized
request to prrr for a named pair. `sy` only **relays**: it carries the request and
the caller's per-use biometric signature to prrr, which remains the enforcement
point (server-side pair authorization + signature verification). `sy` never
enables routing itself.

**DoR:** Steps 1–2 done; prrr ROADMAP Step 4 (authorized remote enable) available.

**DoD:**
- [ ] `ecosystem.request-sharing { pair, signature }` forwards to prrr and returns prrr's accept/deny verdict.
- [ ] `sy` performs no routing change itself; it only relays the request and the signature.
- [ ] An unauthorized or unsigned request is rejected by prrr and surfaced as a clear `sy` error.
- [ ] Tests: a valid relayed request returns prrr's accept and the registry then shows `client_to_client: true` + a published mesh address; an unsigned request is denied end to end.

**Risks:** `sy` becoming a confused deputy for routing changes — mitigate by relaying the caller's signature unmodified and letting prrr enforce; `sy` holds no authority of its own here.
