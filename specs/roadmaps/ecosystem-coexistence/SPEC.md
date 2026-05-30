# SPEC: ecosystem zero-config coexistence (sy · syauth · prrr · cathouse)

## 1. Summary

Four products share one laptop and one phone: **sy** (agentic desktop layer),
**syauth** (phone-as-key unlock over BLE), **prrr** (censorship-resistant
egress VPN), and **cathouse** (Wi-Fi companion that drives `sy` from the phone
and productivizes the phone's sensors). This spec defines the contract by which
they **coexist with zero configuration**: each component, out of the box,
advertises what it is and which exclusive resources it owns, discovers its
siblings, and auto-adjusts so it never conflicts. The only thing a user ever
toggles is an *added capability* (e.g. prrr client-to-client sharing) — never a
conflict-avoidance setting.

`sy` is the coordination hub: it hosts the **ecosystem registry** the other
three register with, exposes `sy ecosystem` for humans and agents, supervises
the desktop daemons under `sy.target`, and runs cross-component conflict checks
in `sy doctor`.

## 2. Principles

1. **Coexistence is the default; only sharing is opt-in.** Avoiding a conflict
   needs no setting. Gaining a capability that crosses a trust boundary
   (prrr routing one client to another) is the single explicit switch.
2. **Declare, discover, defer.** Each component declares the exclusive resources
   it owns, discovers siblings through the registry, and defers on anything a
   sibling already owns.
3. **One device identity.** The phone↔laptop bond is established once and shared;
   every component references the same `peer_id` rather than re-pairing.
4. **The trust boundary stays with each component.** Coordination shares
   *facts* (who owns what, presence, published addresses), never *authority*:
   each component still authenticates and authorizes its own actions.
5. **No-snowflake.** Coordination state lives in the registry and each
   component's shipped config, never in hand-edited host files.

## 3. The ecosystem registry (sy-hosted)

On the **laptop**, each component writes a descriptor on start and updates it on
state change, either by an IPC v1 `ecosystem.register` op against `sy` or by
writing `$XDG_RUNTIME_DIR/sy/ecosystem/<component>.json`. `sy` aggregates and
serves the set read-only over `sy ecosystem [--json]` and an MCP tool.

Descriptor shape:

```json
{
  "component": "prrr | syauth | sy | cathouse",
  "role": "vpn-egress | unlock | orchestrator | companion",
  "owns": ["android:VpnService", "udp:443", "tcp:443", "iface:prrr0"],
  "endpoints": { "uds": "…", "lan": "host:port", "mdns": "_cathouse._tcp" },
  "identity": { "peer_id": "<blake3-of-pubkey>" },
  "flags": { "full_tunnel": true, "client_to_client": false,
             "excluded_cidrs": ["192.168.0.0/16"],
             "unlocked": false, "present": false,
             "mesh_peer_addr": null }
}
```

On the **phone**, the same role is played by a coordination interface the three
apps expose to one another. Because they ship from one developer, they
coordinate through a **signature-permission-protected `ContentProvider`**
(guarded by `android:protectionLevel="signature"`), so only co-signed ecosystem
apps can read each other's descriptors. No user setup.

## 4. Conflict matrix and automatic resolutions

| Contended resource | Owner | Every other component's automatic rule |
|--------------------|-------|----------------------------------------|
| Android `VpnService` slot (one per device) | prrr | syauth (BLE) and cathouse (sockets) never request it |
| BlueZ adapter for the unlock channel | syauth | cathouse never claims BLE; reads presence from the registry |
| TCP/UDP port 443 | prrr | cathouse uses its own port and auto-picks a free one if taken; never binds 443 |
| LAN path to a registered sibling/laptop | — | prrr keeps local-LAN + registered sibling endpoints **out of its tunnel** by default |
| Phone↔laptop device identity / bond | syauth-core bond | one pairing; siblings adopt the `peer_id` from the registry |
| `sy.target` supervision | sy | each desktop daemon ships a `sy`-supervised unit; `sy` auto-discovers it |
| Android Keystore alias | per-app | derived from the app id; cannot collide |
| Persistent foreground-service / battery budget | per-app | independent budgets; the registry lets one app see another's link state to avoid redundant wake |

## 5. Per-component obligations

Each component carries its obligations as a roadmap in its own repo (linked in
§8). In summary:

- **prrr** — (a) by default exclude RFC1918 local-LAN and registered sibling
  endpoints from its tunnel so a sibling's LAN traffic is never captured;
  (b) ship **client-to-client sharing**: operator marks two clients as allowed
  in the server web config, the switch is flipped — `prrr sharing enable` on
  Linux, an "Enable sharing" toggle on Android, **or an authorized request
  relayed from `sy`/cathouse** (decision §9.4) — and prrr then routes the two
  clients to each other **and publishes each peer's mesh address into the
  registry** (`flags.mesh_peer_addr`); (c) register its descriptor and reflect
  `full_tunnel` / `client_to_client` / `excluded_cidrs`. prrr stays the
  enforcement point: a relayed request is honored only when the server-side pair
  authorization exists and the request carries the per-use biometric signature.
- **syauth** — publish the shared bond identity (`peer_id`) and live
  `present`/`unlocked` flags to the registry so a sibling can adopt the pairing
  and optionally gate on unlock; own BLE exclusively; require no change to its
  unlock path.
- **sy** — host the registry + `sy ecosystem` + MCP tool; supervise
  `cathouse-desktopd` (and the existing `syauth-presenced`) under `sy.target`;
  surface a bar pill per component; add cross-component checks to `sy doctor`;
  relay an authorized "enable prrr sharing" request to prrr (decision §9.4).
- **cathouse** — register; adopt the syauth bond from the registry (pair once);
  discover the laptop by LAN mDNS, falling back automatically to
  `flags.mesh_peer_addr` when prrr sharing is on and the LAN path is absent;
  never claim `VpnService`/BLE/443; detect a non-ecosystem full-tunnel VPN
  capturing the LAN path and surface remediation.

## 6. The off-LAN story (resolved by prrr client-to-client)

cathouse is LAN-native. Off-LAN reach is **not** something cathouse builds; it is
a capability prrr gains. prrr is currently egress-only (client↔server, no
client-to-client), so cathouse off-LAN is unavailable. Once prrr client-to-client
ships, enabling sharing for the two clients — locally on prrr **or** via an
authorized request relayed from `sy`/cathouse (decision §9.4) — publishes the
mesh peer address into the registry, and cathouse uses it **automatically** with
no cathouse-side reconfiguration. The enforcement and routing stay inside prrr;
`sy`/cathouse only request and consume, so the ecosystem behaves as one without
moving the trust boundary out of prrr.

## 7. Security & non-functional notes

- **Shared facts, not authority.** The registry carries identity references and
  ownership/flags, never private keys. Each component authenticates its own
  channel (syauth Ed25519/biometric, cathouse mutual-TLS/biometric, prrr PQ
  handshake) regardless of registry contents.
- **Registry integrity.** The laptop registry lives under the per-user
  `$XDG_RUNTIME_DIR`; the phone surface is signature-permission protected. A
  component treats registry data as a hint and still verifies identity on its
  own channel (fail-closed).
- **Degradation.** Any component runs standalone when its siblings are absent;
  registry reads are best-effort and never block core function.
- **Observability.** `sy ecosystem` and `sy doctor` expose the live coexistence
  state and flag conflicts (e.g. a full-tunnel VPN without LAN exclusion while a
  companion is present).

## 8. Per-component roadmaps

- sy — `specs/roadmaps/ecosystem-coexistence/ROADMAP.md`
- prrr — `specs/ecosystem-coexistence/ROADMAP.md`
- syauth — `specs/ecosystem-coexistence/ROADMAP.md`
- cathouse — `specs/companion-platform/ROADMAP.md` (ecosystem folded into the one cathouse roadmap: Steps 7, 15, 16, 17)

## 9. Resolved decisions

These four were settled with the maintainer and are binding on the per-component
roadmaps:

1. **Registry transport — `sy` IPC op with a file fallback.** Components prefer
   the `ecosystem.register`/`update`/`list` IPC v1 ops; a watched
   `$XDG_RUNTIME_DIR/sy/ecosystem/<component>.json` directory covers components
   that start before `sy`. (§3.)
2. **Device identity — syauth owns the bond; others adopt.** The bond lives in
   syauth's store; the registry publishes the `peer_id`; cathouse and prrr
   reference it read-only and derive their own per-service keys. No neutral
   identity store, no second pairing. (§4–§5.)
3. **Android coordination surface — a signature-permission `ContentProvider`.**
   Co-signed ecosystem apps read each other's descriptors through it. (§3.)
4. **prrr sharing is drivable from `sy`/cathouse, not only locally.** The local
   `prrr sharing` CLI/Android toggle remains, *and* `sy`/cathouse can send an
   authorized request that prrr honors (subject to the server-side pair
   authorization). This is the one place control authority deliberately crosses
   into prrr's routing layer; it is gated by the same pair authorization and the
   per-use biometric, and prrr remains the enforcement point. (§5–§6.)

## 10. Remaining open questions

- Should the authorized "enable sharing" request originate from cathouse (phone
  tap) or `sy` (desktop), and which identity signs it?
- For components that start before `sy`, how long is the file-fallback descriptor
  trusted before a liveness re-check?
