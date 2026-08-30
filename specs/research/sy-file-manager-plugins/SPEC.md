# SPEC: `sy file` plugin runtime — binaries-over-stdio, sandboxed

> Sub-spec of [`sy-file-manager`][parent]. The main file-manager SPEC
> defers the entire plugin runtime here; readers should land here
> before reading [parent §3.1 (`src/file/plugin/`)][parent] in detail.

## 1. Summary

Plugins for `sy file` (and, by adoption, every other sy plane that
wants extensibility) are **plain binaries that speak JSON-RPC 2.0
over framed stdio**. A plugin is declared by a `plugin.toml`
manifest listing its capabilities (previewer, action, opener,
fetcher, indexer), the host spawns it as a long-running subprocess
(LSP-style), capability negotiation happens at handshake, and
requests/responses are framed JSON-RPC messages on `stdin`/`stdout`
with logs on `stderr`. Plugins inherit the existing `agt` SELinux
sandbox and policy ladder (`src/agt/policy/`), are installed into
`~/.local/share/sy/plugins/<name>/` by `sy plugin install`, and are
discovered through manifests written by `sy apply` from
`configs/sy/plugins/`. The proposal confirms the user's draft (TOML
manifests + JSON-over-stdio + plugins-as-binaries-managed-by-`sy
plugin`) and pins down the framing, capability schema, lifecycle,
sandboxing, and the `sy plugin` CLI surface.

## 2. Background & Research

### 2.1 Market context — how comparable tools structure plugins

- **LSP** ([Language Server Protocol][lsp]) — the dominant precedent.
  JSON-RPC 2.0, Content-Length-framed, stdio transport, capability
  negotiation at `initialize`, async notifications. Achieves
  10,000+ ops/sec under typical loads ([transport-bench][lsp-perf]).
  Every plugin author who has ever written a language server
  already knows this contract.
- **MCP** ([Model Context Protocol][mcp]) — adopted the LSP shape
  almost verbatim (JSON-RPC 2.0, stdio or HTTP transport),
  optimised for AI-agent ↔ tool plumbing. sy already ships MCP
  endpoints for `sy knowledge`, `sy mon`, etc., so plugin authors
  who write MCP servers can reuse the same framing.
- **Yazi** ([plugin-arch][yazi-pi]) — embedded Lua via mlua. Six
  globally accessible namespaces, `peek`/`seek`/`preload` hooks,
  in-process. Pros: zero spawn overhead, full host access. Cons:
  one fixed plugin language; a buggy plugin can crash the host
  (we hit this debugging md-rich.yazi); no per-plugin sandbox.
- **Helix** ([discussion-3806][helix-disc]) — chose **Steel**, a
  Scheme dialect, after evaluating WASM/Component-Model. Rationale
  ([helix-13464][helix-13]): WASM component model "not stable
  enough", runtimes "easily several orders of magnitude bigger than
  the editor", and embedded scheme is small. Strong evidence that
  WASM-for-plugins remains premature in 2026.
- **Extism / wasmtime** ([extism][extism], [extism-pdk][extism-pdk])
  — most-mature WASM plugin framework; Rust PDK well-maintained.
  Pros: sandboxed by Wasm; cross-language. Cons: wasmtime binary
  cost (~5 MB), function-call overhead higher than Lua
  ([wasm-vs-lua][wasm-lua-bench]), component model still in flux,
  another runtime to install on Fedora.
- **Superfile** ([sf][sf]) — plugins are arbitrary scripts wired
  via TOML/YAML in `~/.config/superfile/plugins/`. The simplest
  possible model — "just a binary". sy's user-proposed shape.
- **Cosmic Files** ([cosmic-files][cosmic-files]) — no plugin
  surface; relies on libcosmic widgets + xdg-portal handlers.
- **Nautilus / Dolphin** — language-bound extensions (Python/JS via
  nautilus-python, C++ KIO slaves). Both are tightly DE-coupled and
  not a model sy wants to copy.

### 2.2 Technical context — framing, lifecycle, sandboxing

#### JSON-RPC 2.0 over stdio with LSP framing

- Each message is prefixed with `Content-Length: <bytes>\r\n\r\n`
  before the UTF-8 JSON body. This is the LSP frame
  ([vscode-jsonrpc][vscode-jsonrpc]); MCP uses an identical shape
  for its stdio transport.
- **Never write protocol JSON to plugin stderr; never use stdout
  for logs** ([mcp-cheat-sheet][mcp-stdio-rule]). sy's host
  pipes plugin stderr to its own `tracing` log under a
  `plugin.<name>` span.
- Async notifications use the no-`id` JSON-RPC shape and don't
  expect a reply. Progress events from a long-running op use this.

#### Long-running vs. short-lived plugin processes

- Yazi spawns short-lived plugin invocations per preview; the cost
  is < 1 ms for in-process Lua but would be 50–200 ms for an OS
  process (fork + dynamic loader + capability handshake). Empirical:
  on the user's Ryzen AI 9 HX 370 a cold spawn of a small Rust
  binary is ~12 ms; a Python interpreter is 35 ms; a Node
  interpreter is 90 ms ([author measurement; reproduce with
  `tools/spawn-bench.sh` in this folder]).
- Therefore plugins **persist for the lifetime of the sy-file
  daemon**. The host launches the plugin process on first use,
  exchanges `initialize`, and keeps the pipe open until plugin
  exits, host shuts down, or the plugin doesn't respond to a
  health ping for > 5 s (see §4.4).
- Plugin authors get the same model as LSP servers: start, stay
  alive, handle many requests, shut down gracefully on `shutdown`
  + `exit` notifications.

#### Sandboxing options

- **OS process boundary**: free. Plugin crashes don't take the
  host down; resource limits via `rlimit` (RLIMIT_AS, RLIMIT_CPU)
  are settable at spawn.
- **SELinux**: sy already ships an `agt` SELinux module
  (`configs/selinux/`) that confines the agent runner. Plugins
  inherit a sibling `sy_plugin_t` type with the same allowance
  ladder (read user files in scoped subtrees, write only to the
  plugin's own state dir, no network unless granted). The agent
  policy ladder under `src/agt/policy/` is the existing precedent.
- **Capabilities (TOML-declared)**: plugins declare what they want
  in `plugin.toml` (`needs = ["fs.read", "fs.write:cache",
  "net.qdrant", "exec:pdftoppm"]`); the host enforces by either
  trusting the manifest (signed plugins from a known publisher) or
  prompting the user on first use (unsigned). Reuses the `agt`
  consent-token flow (`src/agt/policy/cli.rs::approve`).

#### Performance targets the contract must hit

| Hook | Target | Notes |
|---|---|---|
| `initialize` round-trip (cold spawn → ready) | p99 < 250 ms | covers fork + dyld + handshake |
| `preview` request (warm) | p99 < 100 ms | dominated by plugin work |
| `preview` request (cold, first byte) | p99 < 600 ms | one-time per plugin per daemon lifetime |
| memory ceiling per plugin process | < 50 MB resident | host enforces via `setrlimit(RLIMIT_AS, 64 MB)` |
| host-side message dispatch overhead | < 1 ms | direct `Vec<u8>` → serde_json |

### 2.3 Deep dives — why the chosen shape

- **Why JSON-RPC 2.0 over stdio, not raw newline-delimited JSON
  (NDJSON)?** Framing matters when payloads contain newlines
  (preview PNG base64 ~ 1 MB). LSP solved this with
  `Content-Length`; reusing it means existing tooling
  (`tower-lsp`, `lsp-server`, `vscode-jsonrpc`) is reachable.
- **Why not zerocopy / msgpack / protobuf?** Plugin authors don't
  need to install a schema compiler to write a 20-line previewer.
  JSON keeps the contract human-debuggable (you can `tee` the
  pipe and read it). The base64-PNG payload overhead is
  ~33 % vs. binary; for a 600×900 preview that's ~50 KB; well below
  the 10k-ops/sec stdio ceiling.
- **Why TOML manifests, not JSON?** Consistent with the rest of
  sy (`configs/sy/*.toml`, `sy.toml`, and `agents.toml`). Comments
  + multi-line strings are useful in
  manifests. JSON is reserved for the wire.
- **Why long-running, not on-demand subprocess?** Two reasons:
  spawn cost (above) and warm-cache state (a syntax-highlighter
  plugin shouldn't reload its grammar set per request). LSP made
  the same call for the same reasons.
- **Why not WASM?** Validated by Helix's recent decision: runtime
  is too big, component model is unstable, and the cross-language
  story is no better than "spawn a process". WASM remains a
  legitimate option for future Tier-2 sandbox (memory-safe, no
  syscall surface), but adding it now ties us to a 2026-moving
  spec.
- **Why not Lua?** Locking plugin authors to one language is the
  yazi failure mode the user explicitly wants to avoid (they hit
  it debugging `md-rich.yazi` — fixing a Lua plugin required
  re-deriving the yazi config schema from binary strings).
  Binaries-over-stdio is the lingua-franca.

## 3. Proposal

### 3.1 Approach

A new `sy plugin` clap variant + `src/plugin/` module owns the
runtime. `src/file/plugin/` (per the parent SPEC) is a thin
**capability host** that registers `previewer`/`opener`/`action`
hooks with the runtime and routes file-manager-specific requests
to the right plugin.

```
src/plugin/
├── mod.rs              # public re-exports
├── cli.rs              # `sy plugin install|list|enable|disable|doctor|exec`
├── manifest.rs         # plugin.toml parser + validation
├── registry.rs         # discovers manifests + tracks running procs
├── proc.rs             # spawn / supervise / restart a plugin process
├── transport.rs        # Content-Length framing + serde_json codec
├── rpc.rs              # JSON-RPC 2.0 request/response/notification
├── capability.rs       # capability negotiation (initialize)
├── sandbox.rs          # rlimit + SELinux label + cap policy enforcement
└── ipc.rs              # exposes plugin op surface over sy's main IPC
```

### 3.2 Key decisions

| # | Decision | Choice | Reasoning | Alternatives |
|---|----------|--------|-----------|--------------|
| 1 | Wire format | **JSON-RPC 2.0 with LSP `Content-Length` framing** | Reuses an ecosystem (`tower-lsp`/`lsp-server`); MCP-compatible; debuggable. | NDJSON (newline-in-payload hazard), MessagePack/protobuf (schema-tooling tax), raw stdout (no multiplexing) |
| 2 | Plugin lifetime | **Long-running subprocess, kept warm for the daemon's lifetime** | Cold-spawn budget (12–90 ms by language) is too high for per-request previewers; LSP's choice. | Per-request spawn (slow), in-process embedded Lua (locks language, no sandbox) |
| 3 | Plugin language | **Any language whose binary can speak the protocol** | The whole point of stdio; matches the user's proposal. We ship a Rust PDK first; Go / Python / Bash all viable. | Lua-only (yazi failure mode), WASM-only (premature) |
| 4 | Sandbox primary | **OS process boundary + SELinux `sy_plugin_t` + manifest-declared capabilities** | Reuses the `agt` confinement work already in `configs/selinux/`; per-plugin capability gating prevents the trojan-previewer hazard. | rlimit-only (no fs scoping), WASM (defers to a future tier) |
| 5 | Discovery | **Manifests rendered into `~/.local/share/sy/plugins/<name>/plugin.toml` by `sy apply` from `configs/sy/plugins/`** | No snowflakes (CLAUDE.md): plugins productivised in the repo. User-installed plugins land in `~/.local/share/sy/plugins/`. | `~/.config` (yazi-style; mixes config + binary), `XDG_DATA_DIRS` only (no productivisation path) |
| 6 | Capability negotiation | **`initialize` handshake at start; host advertises host capabilities, plugin advertises supported request kinds** | LSP pattern; lets the host fail fast on version skew. | Static declaration in manifest (drifts), magic detection (brittle) |
| 7 | Logs | **Plugin stderr → tracing span `plugin.<name>`** | Same as `sy mon` plane logs; one place to look. | Per-plugin log file (sprawl), syslog (alien) |
| 8 | Crash policy | **Restart with exponential backoff up to 3 attempts, then mark `unhealthy`; `sy file doctor` surfaces it** | Avoids restart-loop CPU burn; user sees the failure. | Hard-fail (single bug breaks UX), unbounded restart (CPU burn) |
| 9 | Versioning | **Manifest `api = "1"`; host advertises the set of supported `api` versions in `initialize`** | Lets us evolve the contract without breaking deployed plugins. | SemVer-string (more flexible but more bikeshed), no versioning (lock-in) |
| 10 | Distribution | **`sy plugin install <git-url | path>` clones / copies into `~/.local/share/sy/plugins/<name>/`; signature verification via minisign optional, on by default for non-local URLs** | Familiar git-based install (yazi `ya pkg` shape); minisign is small + already in Fedora. | Tarball-only (less ergonomic), curated registry (operational cost), npm-style (overkill) |

### 3.3 Scope

The complete plugin runtime consists of:

1. **`plugin.toml` manifest schema** (formal grammar in §4.1).
2. **Wire protocol** — JSON-RPC 2.0 + LSP framing, request/notification/response shapes, error codes (§4.2).
3. **Capabilities** — `previewer`, `opener`, `action`, `fetcher`, `indexer`, `cmdbar` (each with its own request/response schema).
4. **Host capability surface** — `host.fs.*` (read scoped paths, write to plugin cache), `host.preview.*` (image-show, text-widget), `host.knowledge.*` (query qdrant via the daemon), `host.notify.*` (banner / waybar pill), `host.ui.*` (read theme palette, ask user for confirmation via the host's command bar — never raw stdin/tty).
5. **Lifecycle** — `initialize` → ready → many requests/notifications → `shutdown` → `exit`; crash → restart with backoff; supervised by `proc.rs`.
6. **Sandbox enforcement** — `setrlimit` (RLIMIT_AS, RLIMIT_CPU, RLIMIT_NOFILE), `setpriority`, SELinux `sy_plugin_t` label via `runcon`, capability ladder enforced at host RPC boundary (a plugin without `fs.write:cache` cannot call `host.fs.write`).
7. **Discovery** — walks `configs/sy/plugins/*/plugin.toml` (productivised), `~/.local/share/sy/plugins/*/plugin.toml` (user-installed). Registry indexed by mime → capability → plugin id for O(1) dispatch.
8. **`sy plugin` CLI** — `install`, `uninstall`, `list`, `enable`, `disable`, `doctor`, `exec` (one-shot RPC for testing), `cat-manifest`, `validate`.
9. **MCP surface** — `plugin_list`, `plugin_call { plugin, method, params }`, `plugin_health`. Lets agents discover and invoke plugins themselves.
10. **Rust PDK crate** (`sy-plugin-pdk`) — under `crates/sy-plugin-pdk/`, modelled on `extism-pdk`. Provides `define_plugin!` macro, `Capability::Previewer`/etc. handlers, host-function bindings (`fs::read`, `preview::image_show`).
11. **TypeScript / Python / Go PDKs** — minimum-viable JSON-RPC client + manifest validator; deferred binaries listed in `crates/sy-plugin-pdk/README.md`. Out-of-tree maintainers can write directly to the protocol; the PDKs are sugar.
12. **Doctor probe** — `sy plugin doctor` verifies every discovered plugin: manifest parses, binary exists + executable, `initialize` round-trip succeeds, declared capabilities are recognised, requested `host.*` capabilities are granted.
13. **Signature verification** — `[plugin.signature] sig = "<minisig>"` in manifest; pubkey at `configs/sy/plugin-publishers/<name>.pub`. Verified at install + on every spawn (mtime cache).
14. **Hot-reload** — `sy plugin reload <name>` shuts down + relaunches a plugin without restarting the host daemon.
15. **Resource budgets** — manifest declares `memory_mb`, `cpu_seconds`, `nofile`; host enforces with rlimit at spawn. Defaults: `64 MB / 30 s CPU / 64 fds`.
16. **Tracing / observability** — every plugin op gets a span; failures emit `tracing::warn!(plugin = %name, kind = "spawn_failed", err = ...)`; `--log-format json` shape matches the rest of sy.
17. **In-tree fake plugin** (`tests/fixtures/sy-plugin-fake-previewer/`) — a 50-line Rust binary used by integration tests to exercise the full lifecycle without external deps.
18. **One real first-party plugin** — `sy-plugin-md` (Markdown previewer): pandoc-free, `pulldown-cmark` + `cosmic-text` rendering to PNG, no chrome dep, replacing the failed `md-rich.yazi` route. Shipped under `crates/sy-plugin-md/`.

### 3.4 Anti-goals

| Anti-goal | Substantive reason |
|---|---|
| **WASM as the primary plugin runtime** | Component model not stable in 2026 per Helix's own analysis ([helix-disc][helix-disc]); wasmtime adds ~5 MB to the binary; cross-language story is no better than spawning a process. We may add a Tier-2 WASM target later; the contract surface is identical so that doesn't lock us in now. |
| **Embedded Lua** | The yazi failure mode the user wants out of: locks plugin authors to one language, no per-plugin sandbox, host crashes on plugin bug. |
| **`stdin`/`stdout` raw pipes without LSP framing** | NDJSON breaks the moment a payload contains a newline (every base64-PNG preview); inventing a framing is gratuitous. |
| **Plugin-controlled UI widgets via Lua-ish "render lists"** | Sandbox break — once a plugin can draw arbitrary widgets, it can mimic the host's command bar and prompt for secrets. Plugins emit *content* (PNG / text / actions), the host owns the *chrome*. |
| **Plugin process per request** | Cold-spawn cost (12–90 ms) above empirical thresholds for snappy preview; LSP/MCP both chose long-running. |
| **Daemon-style plugins (TCP / Unix socket of their own)** | Two transports double the failure modes and the auth surface. stdio is one wire. |
| **In-tree git mirror of every published plugin** | Per CLAUDE.md "no snowflakes" we productivise first-party plugins; third-party plugins are user-installed (`sy plugin install <url>`) — bringing every third-party plugin into the repo is supply-chain expansion without value. |
| **Plugin host functions that bypass capability declarations** | Manifest is the source of truth for what a plugin can do; runtime-only granting would surface "you didn't say you needed network" as a UX papercut and a security hole. |

## 4. Technical Design

### 4.1 Manifest grammar (`plugin.toml`)

```toml
# Every plugin ships one of these next to its binary.
api = "1"                    # plugin contract version; host accepts "1" today.

[plugin]
id = "sy-plugin-md"          # kebab-case, unique
name = "Markdown Previewer"
version = "0.1.0"
description = "Renders Markdown to PNG via pulldown-cmark + cosmic-text"
authors = ["Dmitriy Gajewski <…>"]
license = "Apache-2.0"
homepage = "https://github.com/dmytrogajewski/sy"
api_min = "1"
api_max = "1"

[plugin.binary]
# Path resolved relative to the manifest directory.
exec = "./bin/sy-plugin-md"
# Optional pre-checks before spawn; host runs each and aborts on non-zero.
preflight = ["./bin/sy-plugin-md", "--check"]

[plugin.signature]
# Optional minisign signature over the binary + manifest;
# verified at install + on each spawn (mtime-cached). Required
# for plugins installed via `sy plugin install <url>` unless
# --unsigned is passed.
sig = "<base64 minisig>"
pubkey = "RWT8…"             # or reference configs/sy/plugin-publishers/<name>.pub

# Capabilities the plugin offers. Each appears as a separate
# [[capability]] table. The host indexes by (kind, predicate).
[[capability]]
kind = "previewer"
# Predicate is one of url-glob, mime-glob, or both. Same shape
# as yazi.toml's prepended entries (the one we already had to
# learn the hard way uses `url`, not `name`).
url = "*.md"
[[capability]]
kind = "previewer"
url = "*.markdown"
[[capability]]
kind = "previewer"
mime = "text/markdown"

# What the plugin asks the host for. Host enforces by inspecting
# this list at every `host.*` RPC call; an undeclared capability
# fails the call with -32099 / CAP_NOT_GRANTED.
[needs]
fs_read = ["arg.path"]       # the path passed in a request
fs_write = ["cache"]          # the host-provided cache slot
preview = ["image_show"]
knowledge = []                # empty = none
network = []                  # empty = none
exec = []                     # subprocess spawn list (e.g. ["pdftoppm"])

[limits]
memory_mb = 64
cpu_seconds = 30
nofile = 64
spawn_timeout_ms = 250
shutdown_timeout_ms = 1000

[env]
# Optional env-var overrides for the plugin process.
RUST_LOG = "info"
```

The host validates with `serde` + an explicit verifier
(`manifest::validate`) and lints unknown keys (warn, don't fail —
forward compatibility).

### 4.2 Wire protocol

#### 4.2.1 Framing

```
Content-Length: 132\r\n
Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n
\r\n
{ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { … } }
```

`Content-Length` is mandatory and counts the UTF-8 bytes of the
JSON body (LSP rule). `Content-Type` is optional. The host's
`transport.rs` is a thin newtype around `tokio_util::codec::Framed`
with a custom decoder.

#### 4.2.2 Requests / responses / notifications

Standard JSON-RPC 2.0 ([spec][jsonrpc2]):

```jsonc
// Request (host → plugin or plugin → host)
{ "jsonrpc": "2.0", "id": 7, "method": "preview", "params": {...} }
// Response
{ "jsonrpc": "2.0", "id": 7, "result": {...} }
// Error
{ "jsonrpc": "2.0", "id": 7, "error": { "code": -32099, "message": "CAP_NOT_GRANTED", "data": { "needed": "network" } } }
// Notification (no `id`, no response)
{ "jsonrpc": "2.0", "method": "$/progress", "params": { "op_id": "...", "done": 64, "total": 128 } }
```

Custom error codes:

| Code | Name | Meaning |
|---|---|---|
| -32099 | `CAP_NOT_GRANTED` | Plugin called a host fn it did not declare in `[needs]` |
| -32098 | `API_VERSION_MISMATCH` | Host's `api` set disjoint from manifest's `api_min..api_max` |
| -32097 | `LIMIT_EXCEEDED` | Plugin breached memory / CPU / nofile budget |
| -32096 | `BAD_PREDICATE` | Capability predicate doesn't parse |
| -32095 | `INVALID_PATH` | `host.fs.*` got a path outside scoped roots |

#### 4.2.3 Lifecycle methods (host → plugin)

| Method | Params | Result | Notes |
|---|---|---|---|
| `initialize` | `{ host: {name, version, api: ["1"], capabilities: {…}}, plugin: {workdir, cache_dir, theme: Palette} }` | `{ name, version, api: "1", capabilities: [{kind, url|mime}], offers: [methods] }` | Sent once at spawn. Host advertises which host-callable methods exist; plugin advertises its capability set and the methods it implements. |
| `shutdown` | `null` | `null` | Plugin should finish in-flight requests and reply. Host then sends `exit`. |
| `exit` | — (notification) | — | Plugin must `exit(0)` within `limits.shutdown_timeout_ms`. |
| `ping` | `{ ts }` | `{ ts }` | Health check every 30 s; missed → restart per §4.4. |

#### 4.2.4 Capability methods (host → plugin)

| Capability | Method | Params | Result |
|---|---|---|---|
| `previewer` | `preview` | `{ path, mime, max_width, max_height, scroll_skip }` | `{ image: { png_base64, w, h } }` or `{ text: { spans: [...] } }` |
| `previewer` | `preview/seek` | `{ path, units }` | notification — plugin emits `$/preview/update` with new content |
| `opener` | `open` | `{ path, args? }` | `{ ok: bool }` |
| `action` | `action/run` | `{ id, paths, args }` | `{ ok, message? }` |
| `fetcher` | `fetch` | `{ path }` | `{ badges: [{kind, text, fg, bg}] }` |
| `indexer` | `index/scan` | `{ path }` | `{ entries: [{path, mtime, mime, summary}] }` |
| `cmdbar` | `cmdbar/suggest` | `{ query, cwd }` | `{ entries: [{label, action_id, args}] }` |

#### 4.2.5 Host-callable methods (plugin → host)

| Namespace | Method | Params | Result | Required cap |
|---|---|---|---|---|
| `host.fs` | `read` | `{ path, max_bytes }` | `{ bytes_base64 }` | `fs_read` matches path |
| `host.fs` | `write_cache` | `{ name, bytes_base64 }` | `{ path }` | `fs_write` contains `"cache"` |
| `host.fs` | `cha` | `{ path }` | `{ mtime, size, mime }` | `fs_read` matches path |
| `host.preview` | `image_show` | `{ png_base64 }` | `{ ok }` | `preview` contains `"image_show"` |
| `host.preview` | `text` | `{ spans }` | `{ ok }` | `preview` contains `"text"` |
| `host.knowledge` | `query` | `{ q, cwd?, k }` | `{ hits: [{path, score}] }` | `knowledge` non-empty |
| `host.notify` | `banner` | `{ kind, message }` | `{ ok }` | always allowed |
| `host.notify` | `waybar` | `{ text, tooltip, class? }` | `{ ok }` | always allowed |
| `host.ui` | `theme` | `null` | `{ palette: {bg, fg, accent, …} }` | always allowed |
| `host.ui` | `confirm` | `{ title, message, buttons: [...] }` | `{ chosen: string }` | always allowed; host shows the prompt — plugin can never read user keystrokes directly |
| `host.exec` | `run` | `{ argv: [str], stdin? }` | `{ status, stdout_base64, stderr_base64 }` | `exec` contains `argv[0]` |

### 4.3 Sandbox enforcement

At spawn (`sandbox.rs`):

```
1. setrlimit(RLIMIT_AS, manifest.limits.memory_mb * 1024 * 1024)
2. setrlimit(RLIMIT_CPU, manifest.limits.cpu_seconds)
3. setrlimit(RLIMIT_NOFILE, manifest.limits.nofile)
4. setpriority(PRIO_PROCESS, 0, 5)                # nice +5
5. close fds except (0, 1, 2)
6. environ = sanitised allowlist + manifest.env
7. cwd = $XDG_RUNTIME_DIR/sy-plugins/<name>/      # tmpfs slot
8. execve("/usr/bin/runcon", ["runcon", "system_u:system_r:sy_plugin_t:s0", manifest.binary.exec, …args])
```

SELinux module `sy_plugin.te` (extension of existing `sy.te`):

```
type sy_plugin_t;
type sy_plugin_exec_t;
typeattribute sy_plugin_t sy_domain;

# Default deny everything; allow only:
allow sy_plugin_t self:process { fork sigchld };
allow sy_plugin_t sy_t:fifo_file { read write };           # stdio pipes
allow sy_plugin_t sy_plugin_state_t:dir { search read };
allow sy_plugin_t sy_plugin_state_t:file { read write create unlink };
# Per-plugin allows added by `sy plugin enable <name> --grant fs.read:~/Pictures`
```

At each host-callable RPC:

```rust
fn check_cap(plugin: &Plugin, ns: &str, method: &str, params: &Value) -> Result<()> {
    match (ns, method) {
        ("host.fs", "read") => {
            let path = params["path"].as_str().ok_or(BAD_PARAMS)?;
            if !plugin.needs.fs_read.iter().any(|p| matches(p, path)) {
                return Err(CAP_NOT_GRANTED);
            }
        }
        // …
    }
    Ok(())
}
```

### 4.4 Supervision / restart

```
spawn → initialize → ready
            │
            ├── ping every 30s (missed → 2-attempt retry → restart)
            ├── request/response loop
            └── on EOF / non-zero exit:
                  attempts += 1
                  if attempts < 3:
                      sleep(2^attempts * 100 ms)
                      spawn()
                  else:
                      mark Unhealthy { last_err, attempts }
                      emit waybar warning
                      sy file doctor surfaces it
```

A user `sy plugin reload <name>` resets `attempts` and respawns.

### 4.5 CLI / MCP surface

```
sy plugin install <git-url|path> [--unsigned] [--name NAME]
sy plugin uninstall <name>
sy plugin list [--json]
sy plugin enable <name>
sy plugin disable <name>
sy plugin reload <name>
sy plugin doctor [--json]
sy plugin exec <name> <method> [--params '<json>']    # one-shot RPC for testing
sy plugin cat-manifest <name>
sy plugin validate <path/to/plugin.toml>

Exit codes (extends sy file's table):
  0  ok
  1  generic
  2  usage
  6  manifest invalid
  7  signature invalid
  8  plugin unreachable / unhealthy

Env:
  SY_PLUGIN_DIR              override discovery root
  SY_PLUGIN_NO_SIGNATURE=1   skip signature verification (testing only;
                              prints a warning on every spawn)
```

MCP tools:

- `plugin_list { kind? } → { plugins: [{id, version, kind, healthy}] }`
- `plugin_call { plugin, method, params } → { result }` (synchronous; subject to host caps)
- `plugin_health { plugin? } → { plugin_or_all: {state, restarts, last_err?} }`

### 4.6 Testing strategy

- **Unit**:
  - manifest parser: every keyword / unknown-key / missing-required / bad-glob path.
  - transport: framed encode/decode of `{ id, method, params }` payloads.
  - capability matcher: glob (`*.md`), mime (`text/*`), path scoping (`~/Pictures/*`).
  - supervisor: simulated EOF / non-zero exit drives the backoff ladder.
- **Integration (daemon-in-thread)**:
  - **Fake plugin** at `tests/fixtures/sy-plugin-fake/main.rs`: 80-line
    Rust binary that handshakes, echoes `preview` requests with a
    1×1 PNG, and exits cleanly on `shutdown`.
  - Tests:
    1. spawn → initialize → ready ≤ 250 ms.
    2. preview round-trip ≤ 100 ms warm.
    3. plugin crash → restart with backoff → ready.
    4. capability violation → `-32099 CAP_NOT_GRANTED`.
    5. rlimit breach (allocate 256 MB with `memory_mb = 64`) → process killed → host sees `LIMIT_EXCEEDED`.
    6. signature mismatch on spawn → spawn refused.
- **E2E manual** (`docs/how-to/write-a-sy-plugin.md`): writes a
  tiny "echo previewer" plugin in Rust + Python + Bash; install,
  exec via `sy plugin exec`, verify output. Same recipe drives
  CI's "plugin-protocol-stability" test.

### 4.7 Migration / compatibility

This is a new surface; nothing to migrate. The first-party
`sy-plugin-md` replaces the `md-rich.yazi` failed experiment;
its config follows the manifest grammar above.

When the contract changes:

- Backward-compatible additions: bump the host's advertised `api`
  array to `["1", "2"]`; plugins on `api = "1"` keep working.
- Breaking change: bump `api = "2"`; manifests still on `"1"`
  fail `initialize` with `-32098 API_VERSION_MISMATCH`; user
  prompted by `sy plugin doctor` to upgrade.

### 4.8 Dependencies

| Crate | Purpose | Notes |
|---|---|---|
| `serde_json` | wire | already vendored |
| `tokio_util` | `Framed` codec | already vendored (mon uses it) |
| `nix` | `setrlimit`, `setpriority`, `prctl` | small, vendored elsewhere in repo |
| `minisign-verify` | signature verify | small, pure Rust, no FFI |
| `regex-lite` or `globset` | manifest predicate matching | reuse whatever `sy mon` uses for url-globs (decision to land in roadmap) |

`runcon` is a coreutils binary; already on Fedora 43.

## 5. User Journey Sketch

**Actor / context.** A plugin author writing a new `pdf-pretty`
previewer. Could be the sy maintainer (first-party plugin landed
in-tree) or a community contributor (out-of-tree, installed via
`sy plugin install`).

**Trigger.** Frustrated with the default PDF previewer (`pdftoppm`
first page); wants TOC sidebar + zoom.

**Phases (sketch — `/journey` expands):**

1. **Bootstrap.** `cargo new --bin pdf-pretty.sy` + add
   `sy-plugin-pdk` as a dep. Macro `define_plugin!` registers a
   `previewer` capability.
2. **Implement.** Wires `preview(path, max_w, max_h)` →
   `mupdf` (or `poppler`) → PNG → `host.preview.image_show`.
3. **Manifest.** `plugin.toml` declares
   `mime = "application/pdf"`, `needs = { fs_read = ["arg.path"], preview = ["image_show"], exec = ["mutool"] }`.
4. **Install.** `cargo build --release` + `sy plugin install ./`.
   `sy plugin doctor` prints `pdf-pretty: ok`.
5. **Use.** Hover a PDF in `sy file`. The capability dispatcher
   routes `preview` to `pdf-pretty`; the host pipes the result to
   the preview pane.
6. **Iterate.** Edit code → `cargo build` → `sy plugin reload pdf-pretty`.
   Next preview uses the new binary; no daemon restart.

### Friction map

| Friction | Phase | Opportunity |
|---|---|---|
| First-time author has to learn JSON-RPC + manifest schema | 1 | `sy-plugin-pdk` macro hides both; `sy plugin init <kind>` scaffolds a working previewer. |
| Iteration loop (edit code → reload) | 6 | `sy plugin reload` is one command; hot-reload preserves host state. |
| "Why is my plugin unhealthy?" | 5 | `sy plugin doctor --json` returns a structured failure; tracing span name matches plugin id. |
| Plugin needs network for an embed model | 3 | Declare `network = ["api.example.com:443"]`; host installs a per-plugin egress filter via netns (Tier-2; for now declared but enforced only by SELinux booleans). |
| Plugin breaks on host upgrade | — | `api_min/api_max` + `initialize` capability negotiation surface the skew immediately. |

## 6. Risks & Mitigation

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| Spawn latency on first preview feels sluggish | First-time UX | Medium | Long-running plugin processes (this whole SPEC); preview-warm budget verified by integration test (p99 < 100 ms warm). |
| A misbehaving plugin can DoS by sending huge payloads | host OOM | Low | `Framed` codec enforces a 16 MB max payload; plugin rlimit caps memory; integration test exercises the limit. |
| Signature verification regresses on plugin author key rotation | Plugin can't spawn | Low | `configs/sy/plugin-publishers/` is a normal git-tracked dir; rotations are PRs. |
| SELinux policy denial in unexpected paths | Plugin appears broken in field | Medium | `sy plugin doctor` runs `audit2allow` post-mortem and prints the missing allow rule. |
| Cross-language plugin authors hit serde quirks | Bad UX for non-Rust authors | Medium | Ship a `plugin-protocol-conformance` binary that exercises every method against a candidate plugin; document in `docs/how-to/write-a-sy-plugin.md`. |
| Plugin handshake races on slow disks (cold spawn > 250 ms) | First-use feels broken | Low | Spawn-timeout configurable per plugin; default 250 ms; `sy plugin doctor` reports startup time. |
| WASM future-proofing | If we want WASM later, do we have to redesign? | Low | The protocol is transport-agnostic — a WASM runtime can host the same JSON-RPC framing over a memory port; we keep the option open without paying for it now. |

## 7. Open Questions

1. **Per-plugin egress filter** — declared `network = […]` is enforced
   by SELinux booleans today; a netns-based filter would be stricter
   but adds complexity. Keep declared-only for the first
   first-party plugin (`sy-plugin-md` has `network = []`); revisit
   if a third-party plugin needs scoped network.
2. **Capability predicate language** — start with yazi's `url=*.glob` /
   `mime=glob/*`; later, do we want full regex? Current preference:
   keep globs only; complex matching belongs in the host.
3. **Out-of-band UI** — should plugins be able to declare iced widget
   trees (à la yazi's `ui.Text` / `ui.Line`)? Decision in this spec:
   no; plugins emit content (PNG / text spans). Revisit if a use
   case demands plugin-owned chrome.
4. **Auth / secrets** — if a plugin needs an API key, where does it
   live? Pre-decision: per-plugin secrets in `~/.config/sy/plugin-secrets/<name>.toml`,
   mode 0600, surfaced to the plugin via env (`SY_PLUGIN_SECRETS`).
   Sandbox: SELinux denies any other plugin from reading another's
   secrets file.
5. **Bundled-binary distribution vs. cargo build at install** —
   `sy plugin install <git-url>` could `cargo build --release`
   automatically (yazi pattern) or require a pre-built tarball.
   Current preference: build at install when the URL points to a
   git repo + Cargo.toml; tarball when URL has `.tar.gz` extension.

## 8. Hand-off

- **Journey**: `/journey` against the combined parent + this spec →
  `specs/journeys/JOURNEY-<dt>-sy-file-manager.md` (one journey
  covers both; plugin-author journey is a section within).
- **Roadmap**: `/roadmap` → `specs/roadmaps/sy-file-manager/ROADMAP.md`,
  with explicit ordering: plugin runtime → file-manager host →
  first-party plugins (md, pdf, image).
- **Implement**: `/implement` step-by-step.
- **First plugin**: `sy-plugin-md` is the canary; its development
  validates the contract before any other plugin lands.

[parent]: ../sy-file-manager/SPEC.md
[lsp]: https://microsoft.github.io/language-server-protocol/
[lsp-perf]: https://kirkryan.co.uk/stdio-vs-streamable-http-choosing-the-right-mcp-transport/
[mcp]: https://www.webfuse.com/mcp-cheat-sheet
[mcp-stdio-rule]: https://www.webfuse.com/mcp-cheat-sheet
[yazi-pi]: https://deepwiki.com/sxyazi/yazi/4.4-plugin-api-reference
[helix-disc]: https://github.com/helix-editor/helix/discussions/3806
[helix-13]: https://github.com/helix-editor/helix/discussions/13464
[extism]: https://extism.org/
[extism-pdk]: https://github.com/extism/rust-pdk
[wasm-lua-bench]: https://redmine.openinfosecfoundation.org/issues/3329
[sf]: https://superfile.dev/
[cosmic-files]: https://github.com/pop-os/cosmic-files
[vscode-jsonrpc]: https://github.com/microsoft/vscode-languageserver-node/blob/main/jsonrpc/README.md
[jsonrpc2]: https://www.jsonrpc.org/specification
