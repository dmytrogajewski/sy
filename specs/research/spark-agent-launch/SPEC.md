# SPEC: Spark agent launcher

## 1. Summary

Add `sy spark <host> launch <codex|claude|opencode>` as a workstation-side
launcher for coding agents backed by a model served on the configured DGX
Spark. The command preserves Ollama's useful launch semantics—model selection,
readiness, configure-only operation, launch-owned configuration, explicit
argument forwarding, installation discovery, and attached terminal I/O—while
retaining sy's pinned-TLS, scoped-token, immutable-model, resource-admission,
and no-arbitrary-remote-command contracts.

Request: reproduce the behavior of `ollama launch` for Codex, Claude Code, and
OpenCode against `sy spark` model endpoints.

Type: feature.

Actor: a workstation user in a terminal.

Surface: `sy spark <host> launch` plus launch-owned workstation state.

Success looks like: one command ensures a verified model is healthy on Spark,
configures only the selected local agent for the lifetime of the launch, and
runs that agent against the authenticated Spark endpoint without exposing an
administrator credential or changing unrelated agent configuration.

## 2. Background and research

### Market context

#### Ollama launch

Ollama positions `ollama launch` as one-command setup and execution for coding
agents. The initial announcement covers Claude Code, OpenCode, Codex, and Droid,
an optional model override, and configure-only operation. Current source is
substantially more precise than the announcement: it has an integration
registry, saved selections, interactive versus headless policy, model readiness
and pull policy, per-integration adapters, restore support, install discovery,
and `--` argument forwarding. Sources:

- [Ollama launch announcement](https://ollama.com/blog/launch)
- [Ollama launch source at the researched commit](https://github.com/ollama/ollama/tree/f6c59d87038ae77f52d4adfbdc37363f8edd1ef3/cmd/launch)
- [Root command implementation](https://github.com/ollama/ollama/blob/f6c59d87038ae77f52d4adfbdc37363f8edd1ef3/cmd/launch/launch.go)
- [Integration registry](https://github.com/ollama/ollama/blob/f6c59d87038ae77f52d4adfbdc37363f8edd1ef3/cmd/launch/registry.go)

The source was cloned at commit
`f6c59d87038ae77f52d4adfbdc37363f8edd1ef3` (2026-08-24) and its launch package
passed `go test ./cmd/launch` locally.

#### LM Studio CLI and headless runtime

LM Studio separates model lifecycle from clients: `lms get`, `lms load`,
`lms ps`, and `lms server start` operate the runtime, while consumers connect to
an OpenAI- or Anthropic-compatible endpoint. It supports remote model loading
through `--host`, on-demand loading, context and GPU controls, and an explicit
identifier for the API-facing model name. It does not provide Ollama's
per-coding-agent launcher, so users still configure the downstream client.
This supports keeping Spark lifecycle in the control plane while making launch
a thin local adapter. Sources:

- [LM Studio CLI](https://lmstudio.ai/docs/cli)
- [Remote and resource-aware model loading](https://lmstudio.ai/docs/cli/local-models/load)
- [Headless API runtime](https://lmstudio.ai/docs/developer)

#### Direct Claude Code gateway configuration

Claude Code officially supports an LLM gateway through
`ANTHROPIC_BASE_URL`, bearer presentation through `ANTHROPIC_AUTH_TOKEN`, and
explicit primary, Opus, Sonnet, Haiku, and subagent model mapping. A startup
`--model` has higher precedence than environment or settings. This makes a
process-local environment the native integration primitive; editing user or
project settings is unnecessary. Sources:

- [Claude Code environment variables](https://code.claude.com/docs/en/env-vars)
- [Claude Code model configuration](https://code.claude.com/docs/en/model-config)
- [Claude Code gateway configuration](https://docs.anthropic.com/en/docs/claude-code/llm-gateway)

#### Direct Codex custom-provider configuration

Codex supports named custom model providers with a base URL, credential
environment key, and Responses wire protocol. Ollama's current adapter writes a
launch-owned profile and model catalog, invokes Codex with that profile and
model, supplies the credential only in the child environment, and rejects extra
arguments that would override its managed provider/model keys. Current Codex
source has removed Chat Completions support from the provider wire and requires
Responses for custom providers. Sources:

- [Ollama Codex adapter](https://github.com/ollama/ollama/blob/f6c59d87038ae77f52d4adfbdc37363f8edd1ef3/cmd/launch/codex.go)
- [Codex model-provider source](https://github.com/openai/codex/blob/main/codex-rs/model-provider-info/src/lib.rs)
- [Codex generated configuration schema](https://github.com/openai/codex/blob/main/codex-rs/core/config.schema.json)

#### Direct OpenCode provider configuration

OpenCode accepts an inline configuration through
`OPENCODE_CONFIG_CONTENT`. Ollama uses that mechanism so the launch does not
overwrite the user's `opencode.json`; it supplies a custom OpenAI-compatible
provider, model catalog, and selected model for the child process. OpenCode's
provider documentation confirms path-prefixed base URLs, environment-backed
API keys, model IDs, modalities, tool capability, and context/output limits.
Sources:

- [Ollama OpenCode adapter](https://github.com/ollama/ollama/blob/f6c59d87038ae77f52d4adfbdc37363f8edd1ef3/cmd/launch/opencode.go)
- [OpenCode provider configuration](https://opencode.ai/docs/providers)
- [OpenCode custom model metadata](https://opencode.ai/v2/docs/models)

### Technical context: observed Ollama semantics

The researched command has this relevant surface:

```text
ollama launch [INTEGRATION] [--model MODEL] [--config] [--restore] [-y]
              [-- EXTRA_ARGS...]
```

For a named integration it:

1. verifies the Ollama server unless the operation is restore-only;
2. distinguishes TTY from headless execution;
3. resolves an explicit model, a saved model, or an interactive selection;
4. prompts to pull a missing model, auto-pulls with `--yes`, and fails rather
   than prompts in non-interactive mode without approval;
5. saves launch-owned model selection;
6. checks or offers installation according to the integration registry;
7. prepares only the selected integration's environment or owned files;
8. returns after configuration with `--config`, otherwise spawns the agent with
   inherited stdin/stdout/stderr;
9. accepts integration arguments only after `--`; and
10. rejects forwarded Codex arguments that conflict with the provider, profile,
    model, or model-catalog keys managed by the launcher.

Adapter behavior at the researched commit:

| Integration | Endpoint/config mechanism | Invocation | Persistent effects |
|---|---|---|---|
| Claude Code | process environment: Anthropic base URL, bearer, tier/subagent model mappings, telemetry/feedback reductions | `claude --model <model> ...` | saved launcher selection only |
| Codex | launch-owned provider profile and model catalog; token in environment | `codex --profile <owned> -m <model> ...` plus defensive inline provider overrides | two launch-owned files; `--restore` removes only them |
| OpenCode | `OPENCODE_CONFIG_CONTENT` with a custom provider and model entries | `opencode ...` | model-recency state plus saved launcher selection; user provider config remains untouched |

### Fit to sy

Ollama's local pull cannot be copied literally. Spark downloads are immutable
Hugging Face snapshots with an explicit repository and revision, and engine
start is guarded by signed recipes, memory/disk admission, telemetry freshness,
and a high-memory transition lease. `launch` therefore reuses a healthy exact
instance or calls the existing `serve` operation for an already downloaded
model. If the model is absent, it fails with the exact `sy spark download`
remediation instead of guessing a repository or mutable revision.

Ollama's fixed local token also cannot be copied. The Spark bootstrap credential
is an administrator secret and must never enter an agent process. The launcher
must provision and persist a separate `inference`-only bearer credential with
0600 permissions, validate its server-side token ID before use, and inject only
that token into the child environment.

## 3. Proposal

### Approach

Build a workstation-only `spark::launch` module with a small registry of three
known adapters. It orchestrates existing Spark HTTP operations, owns a private
selection/credential state file, renders an integration-specific launch plan,
and starts a local child process with inherited terminal descriptors. No new
Spark daemon route or executor method is required.

### Key decisions

| Decision | Choice | Reasoning | Alternatives |
|---|---|---|---|
| Execution location | Launch Codex, Claude Code, or OpenCode on the workstation | Agents need the current working tree and local terminal; Spark remains the inference appliance | SSH-execute the agent on Spark, rejected because it loses local workspace context and creates an arbitrary-command path |
| Model readiness | Reuse a healthy exact instance; otherwise call existing `serve` and follow it to healthy | Reproduces Ollama's ensure-ready behavior through Spark's signed and admitted lifecycle | Require a separate manual `serve`, rejected because it defeats one-command launch |
| Missing model | Fail with immutable download remediation | Repository and revision cannot be safely inferred from a friendly alias | Implicit mutable Hugging Face download, rejected because it breaks provenance and durability |
| Child credential | Dedicated persisted inference-only token, never the bootstrap administrator token | Gives one-command behavior without granting lifecycle/admin authority to an untrusted coding agent | Reuse admin token; require manual export; mint a token on every launch |
| Agent configuration | Process-local env for Claude/OpenCode; launch-owned profile/catalog for Codex | Matches each client's supported primitive and avoids overwriting user/project config | Rewrite primary user configs; shell snippets requiring manual export |
| Argument forwarding | Only tokens after `--`, with Codex conflict rejection | Matches Ollama and prevents launcher-owned routing from being overridden accidentally | Accept arbitrary trailing tokens before `--`; shell command strings |
| Selection state | Per host and integration under sy's private config directory | A no-`--model` launch can reuse the last exact choice without cross-host confusion | Global selection; mutate client-native defaults |

### Scope

- `launch` CLI with `codex`, `claude`, and `opencode` integration values.
- `--model`, `--config`, `--restore`, `--yes`, `--dry-run`, `--json`,
  `--config-dir`, and `-- EXTRA_ARGS...` behavior.
- TTY model selection from verified Spark models when no explicit or saved
  selection is usable.
- Headless failure when a model decision or installation confirmation is
  required; `--yes` never guesses a model.
- Exact model resolution across model ID, canonical identity, repository, and
  alias, with ambiguity rejected.
- Healthy instance reuse and deterministic launch-owned instance naming when a
  start is required.
- Existing `serve` operation reuse and terminal-state following.
- A private launch-state document that records host, integration, model,
  instance, and inference token ID, but never bearer material.
- A separate 0600 inference bearer file; server-side validation and safe
  replacement if missing, revoked, expired, or mismatched.
- Integration binary discovery, actionable install instructions, interactive
  confirmation for Ollama-equivalent Claude/OpenCode installers, and no prompt
  in headless mode.
- Claude Code environment adapter using the native Anthropic route and all
  primary/tier/subagent mappings.
- Codex launch-owned profile and model catalog using the Responses route,
  conflict validation, backup-safe writes, and owned-file restore.
- OpenCode inline provider/model configuration, without editing the user's
  provider configuration.
- Inherited terminal I/O, current working directory, environment, signals, and
  child exit status.
- Redaction guarantees for debug output, errors, JSON, and process arguments.
- Stable JSON launch-plan document for dry-run/config inspection.
- Unit, integration, black-box CLI, and real `dgx-spark` verification.
- README, CLI reference, Spark reference, and journey documentation.

### Anti-goals

- No remote shell or arbitrary executor command: agent execution belongs on the
  workstation and Spark remains a closed inference/control appliance.
- No implicit mutable model download: a friendly model name lacks the immutable
  repository/revision evidence required by Spark.
- No bootstrap/admin token in a child process, argument, output, or generated
  client file.
- No permanent rewrite of `~/.claude/settings.json`, the user's main
  `~/.codex/config.toml`, or `opencode.json`.
- No generic executable name or shell string: only the three registered
  integrations can be selected.
- No promise that every OpenAI/Anthropic-compatible model is agent-capable;
  signed recipe capabilities and the existing exact identity probe remain the
  authority.

## 4. Technical design

### Architecture

```text
sy spark HOST launch INTEGRATION --model MODEL
  |
  +-- SparkClient over pinned HTTPS + admin credential
  |     +-- list verified models and managed instances
  |     +-- reuse healthy exact instance, or serve + follow operation
  |     `-- ensure one inference-only token exists
  |
  +-- launch-owned local state (0600)
  |     +-- model/instance selection, token ID (TOML)
  |     `-- inference bearer (separate 0600 credential file)
  |
  +-- integration adapter
  |     +-- Claude: child environment
  |     +-- Codex: owned profile/catalog + child environment
  |     `-- OpenCode: inline JSON + child environment
  |
  `-- local child process with inherited TTY and current directory
        `-- HTTPS inference only -> Spark gateway -> private engine bridge
```

Affected modules:

- `src/spark/launch.rs`: registry, resolution, state, token provisioning,
  adapters, command plan, child execution.
- `src/spark/cli.rs`: CLI types and dispatch.
- `src/spark/client.rs`: narrowly exposed profile/credential helpers and
  existing token/lifecycle calls; no duplicate HTTP stack.
- `src/spark/mod.rs`: module registration.
- `tests/spark_launch_e2e.rs`: black-box launch with fake agent executables and
  a real TLS/control-plane fixture.
- docs/specs listed under Scope.

### Launch state

`<config-dir>/spark-launch.toml` is private and versioned:

```toml
schema = "sy.spark.launch-state/v1"

[hosts.dgx-spark]
inference_token_id = "01..."

[hosts.dgx-spark.integrations.codex]
model = "ornith-1.5:9b"
instance = "launch-ornith-1-5-9b-<short-hash>"
```

Bearer material lives only in
`<config-dir>/credentials/spark/dgx-spark.launch-inference`, mode 0600. State
writes use create-new staging, fsync, atomic rename, and directory fsync, reusing
the existing protected-write discipline. JSON output includes `token_id` but
never the bearer.

Token provisioning uses the existing token API with:

- name `sy-launch@<local-hostname>`;
- exactly the `inference` scope;
- no lifecycle, model, operation, log, benchmark, or admin scope; and
- a bounded per-token inference concurrency matching the gateway/client agent
  fan-out contract.

If state and credential disagree, the known old token is revoked when possible
before replacement. Replacement is staged locally only after the create
operation succeeds and returns bearer material.

### Model and instance resolution

1. Resolve the requested/saved value against the verified model list.
2. Accept one exact match by ID, canonical identity, repository, or alias.
3. Reject zero matches with an immutable `download` example and reject multiple
   matches with their non-secret identities.
4. Find healthy published instances whose `model_id` equals the resolved model.
5. Prefer the saved exact instance when still healthy, then the deterministic
   launch-owned instance, then a single other healthy instance. Reject ambiguous
   remaining candidates rather than silently switching endpoints.
6. If none is healthy, call `serve` with a deterministic name, objective
   `agent`, no unverified override, and follow the operation.
7. Re-fetch instances and require the exact model ID, expected name, healthy
   state, and published endpoint before launching the child.

Dry-run performs model/instance resolution and the existing admission plan, but
does not create a token, write state/config, install a client, start an engine,
or execute an agent.

### Integration adapters

#### Claude Code

- Discover `claude` through `PATH`, `~/.local/bin/claude`, then
  `~/.claude/local/claude`.
- Launch with `--model <model>` followed by explicit extra arguments.
- Set `ANTHROPIC_BASE_URL` to `/anthropic/<instance>`.
- Set `ANTHROPIC_AUTH_TOKEN` to the inference bearer and remove inherited
  `ANTHROPIC_API_KEY` so it cannot override bearer routing.
- Map Opus, Sonnet, Haiku, and subagent defaults to the selected model.
- Set `NODE_EXTRA_CA_CERTS` to the pinned Spark CA.
- Disable nonessential/error/feedback traffic and experimental beta headers
  using supported variables already reflected by Ollama and sy's existing
  client projection. These are fixed protocol-adapter invariants, not
  operator-selectable model or engine policy.

#### Codex

- Require the installed minimum compatible Codex version and provide the
  official install command if absent/old.
- Write `~/.codex/sy-spark-launch.config.toml` and
  `~/.codex/sy-spark-launch-models.json`; never rewrite the main config.
- Provider base URL is `/openai/<instance>/v1`, wire is `responses`, and the
  provider credential key is `SY_SPARK_INFERENCE_TOKEN`.
- Launch using the owned profile, model, and defensive `-c` provider overrides.
- Reject forwarded profile/model/provider/catalog overrides.
- Set `OPENAI_API_KEY`/`SY_SPARK_INFERENCE_TOKEN` only in the child environment
  as required by the installed Codex provider contract, plus `SSL_CERT_FILE`
  for the pinned CA.
- `--restore` removes only the two sy-owned files after validating that their
  contents are sy-owned; it leaves the main Codex config untouched.

#### OpenCode

- Discover `opencode` through `PATH`, then `~/.opencode/bin/opencode`.
- Build `OPENCODE_CONFIG_CONTENT` with provider ID `sy-spark`, the path-prefixed
  OpenAI-compatible URL, an environment-backed API key, the selected model, and
  exact available modality/context/output metadata where the signed recipe
  exposes it.
- Set `NODE_EXTRA_CA_CERTS` and the inference credential only in the child
  environment.
- Do not overwrite `opencode.json`; the inline configuration takes precedence
  only for the launched process.

### CLI and JSON surface

```text
sy spark <HOST> launch <codex|claude|opencode> [OPTIONS]
  --model <MODEL>          explicit verified Spark model or alias
  --config                 prepare and save selection without launching
  --restore                restore launch-owned client state where supported
  --yes                    approve safe local client installation prompts
  --dry-run                resolve and run admission without mutation
  --json                   emit sy.spark.launch-plan/v1 for dry-run/config
  --config-dir <PATH>      SY_SPARK_CONFIG_DIR
  -- <EXTRA_ARGS...>       pass exact argv tokens to the local integration
```

Additional environment mappings:

- `SY_SPARK_LAUNCH_MODEL`
- `SY_SPARK_LAUNCH_CONFIG`
- `SY_SPARK_LAUNCH_RESTORE`
- `SY_SPARK_YES`
- `SY_SPARK_DRY_RUN`
- `SY_SPARK_JSON`
- `SY_SPARK_CONFIG_DIR`

Rules:

- `--restore` cannot combine with model/config/dry-run/extra args.
- `--json` cannot combine with an attached agent launch because the child owns
  stdout; use `--dry-run --json` or `--config --json`.
- Extra args without `--` are a usage error.
- Non-TTY execution requires `--model` unless a valid saved selection exists;
  `--yes` does not choose a model.
- Child exit status is propagated. Signal termination maps to a stable generic
  failure when the platform provides no numeric exit code.

Stable exit codes:

- 0: configuration/restore succeeds or child exits successfully.
- 1: unexpected internal failure or child signal termination.
- 2: CLI, local state, missing client, conflicting forwarded args, or ambiguous
  model/instance.
- 3: Spark model/recipe/admission/operation policy rejection.
- 4: Spark unreachable, authentication failure, or TLS pin mismatch.
- For an ordinary numeric child exit code in 1..125, propagate that code.

### Non-functional requirements

- Performance: when a healthy instance and token already exist, launcher
  overhead before process spawn is bounded to two control-plane reads and one
  local config transaction; it must not probe the private engine directly.
- Reliability: all local writes are atomic; failed serve/token/config work never
  replaces the last usable selection; post-serve identity is revalidated.
- Security: no secret in argv, stdout, stderr, JSON, state metadata, tracing, or
  generated files; child receives inference scope only; pinned CA remains
  mandatory; forwarded Codex routing conflicts are rejected.
- Observability: structured fields include host alias, integration, model ID,
  instance ID/name, reused/started decision, token ID, and child exit status;
  bearer material is represented only as `[REDACTED]`.
- Durability: saved selection is host-scoped; token/config state survives sy
  upgrades; schema mismatch fails closed with remediation rather than truncation.

### Testing strategy

Unit tests:

- clap grammar, `--` boundary, flag conflicts, env precedence, and exit mapping;
- exact/zero/ambiguous model and instance resolution;
- deterministic instance naming and bounded normalization;
- private state encode/decode, atomic permissions, schema rejection, and bearer
  redaction;
- inference-only token request and reconciliation decisions;
- Claude env/argv, Codex profile/catalog/argv/conflict checks, and OpenCode inline
  JSON/env;
- install discovery and headless confirmation policy.

Integration tests:

- fake agent binaries capture argv, selected environment keys, current
  directory, stdin, and exit status without exposing secret values;
- a TLS fixture exercises model list, instance list, token create/list/revoke,
  admission, serve/follow, and post-serve revalidation through `SparkClient`;
- negative cases cover TLS mismatch, revoked token, failed operation, stale
  instance, malformed state, config ownership mismatch, and child failure.

End-to-end verification:

- build and install the workstation binary;
- use real `dgx-spark` with the protected stack unchanged;
- dry-run a stopped verified generation model and inspect admission;
- configure all three installed local clients against one healthy model;
- invoke each through a bounded non-interactive/version or one-request path that
  proves argv/environment/config routing without granting broad filesystem work;
- verify the child request reaches the authenticated OpenAI Responses or
  Anthropic Messages gateway as appropriate;
- confirm `ps` retains the healthy model and that no Docker restart, host reboot,
  DGX update, or protected-stack mutation occurred.

### Migration and compatibility

Launch orchestration adds local state only and does not change the Spark
database, control HTTP API, OpenAPI document, executor protocol, recipe schema,
or engine containers. The current Claude Code compatibility lane also extends
the existing Anthropic gateway adapter: Claude Code 2.1.241 emits a trailing
`system` message and a JSON-schema title request, so the gateway promotes system
fragments to one leading message and maps the schema to vLLM
`response_format`. Launch state has an explicit schema and can be removed
without affecting model or instance durability. Existing user agent
configuration is neither imported nor rewritten.

### Dependencies

No new Rust crate or system library is required. Existing clap, serde/TOML/JSON,
reqwest, UUID, SHA-256, filesystem, and process primitives cover the feature.
Interactive selection can use bounded stdin/stderr prompts already available to
the single binary; it does not justify a second TUI dependency.

## 5. User journey sketch

1. The user runs `sy spark dgx-spark launch codex --model ornith-1.5:9b` in a
   project directory.
2. sy authenticates to the pinned Spark control plane, resolves the verified
   model, and reuses or starts its admitted instance.
3. sy ensures an inference-only credential and prepares the Codex launch-owned
   profile/catalog without touching the user's normal provider.
4. Codex opens in the same terminal and working directory; its model calls use
   the Spark Responses route.
5. Codex exits and sy returns its status. A later launch reuses the saved model,
   token, instance, and config.

### Friction map

| Friction | Journey point | Opportunity |
|---|---|---|
| Friendly model is not downloaded | resolution | print an exact immutable `download` command using the requested value and required revision placeholder |
| Cold model takes time and may fail admission | readiness | stream durable operation progress and preserve the precise refusal/problem code |
| Local agent is absent | configuration | interactive install offer where Ollama supports it; deterministic install hint in headless mode |
| User has custom client configuration | configuration | process-local env or sy-owned files only; reject conflicting forwarded routing flags |
| Credential could leak into an agent | launch | mint a least-privilege token and keep it out of arguments, output, and persistent client config |
| Multiple instances match one model | resolution | prefer saved/launch-owned identity and reject unresolved ambiguity explicitly |

North star: from any local project, one command opens the chosen coding agent
against a verified, healthy Spark model with no manual exports, no secret
exposure, and no persistent drift in the user's ordinary agent setup.

## 6. Risks and mitigation

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| Client configuration formats evolve | launch breaks after client update | Medium | version probes, adapter-level golden tests, launch-owned isolated config, actionable compatibility errors |
| Inference credential leaks through diagnostics | unauthorized model use | Low | separate scope, private file, env-only injection, redaction tests, never include env values in errors |
| `serve` succeeds but route is stale/wrong | agent talks to wrong model | Low | re-fetch and verify exact model ID, instance name, health, and endpoint after terminal operation |
| Ctrl-C/signal behavior differs across child CLIs | poor terminal experience | Medium | inherited terminal/process group, no pipes, child-status tests, avoid a proxy loop after spawn |
| Model is tool-incompatible despite serving | broken agent behavior | Medium | signed recipe capability gate and existing exact protocol/model identity probe; surface recipe remediation |
| Persistent launch state drifts from server | confusing reuse | Medium | validate model, instance, and token ID every launch; atomically converge only after success |

## 7. Open questions

None block implementation. The requested Ollama semantics and the existing Spark
security/lifecycle contracts determine the choices above.

## 8. Hand-off

- Expand the journey under `specs/journeys/`.
- Create an ordered implementation roadmap if the journey reveals more than one
  independently testable delivery unit.
- Implement through micro-TDD, then run `make lint`, `make test`, focused launch
  integration tests, and the real-Spark recipe above.

## 9. Implementation evidence

- Ollama launch semantics were traced at commit
  `f6c59d87038ae77f52d4adfbdc37363f8edd1ef3`; its `cmd/launch` package tests
  passed before sy design work began.
- The installed workstation clients used for compatibility were Codex 0.149.1,
  Claude Code 2.1.241, and OpenCode 1.18.21.
- A live Claude request-shape capture retained only field/type metadata. It
  proved the `user, system` role sequence and JSON-schema title call now covered
  by exact gateway regressions; prompt and bearer content were not retained.
- On `dgx-spark`, bounded real requests returned `SPARK_CODEX_OK`,
  `SPARK_CLAUDE_OK`, and `SPARK_OPENCODE_OK` through Responses, Anthropic
  Messages, and OpenAI-compatible routes respectively, all against the same
  healthy exact Ornith instance.
- The Claude gateway correction was delivered as a signed side-by-side ARM64
  control-plane release. The transaction retained its rollback predecessor and
  did not update or restart the DGX OS, kernel, NVIDIA driver, CUDA, Docker,
  firmware, or engine container.
