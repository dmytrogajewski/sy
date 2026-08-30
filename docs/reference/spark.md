<!-- Template source: Good Docs Project reference template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/reference. Diátaxis quadrant: reference. -->

# Spark reference

`sy spark <host>` drives a Spark appliance from your laptop. Use this
page for admission numbers, gateway paths, and engine-policy rules. For
install steps see [How to install the Spark agent](../how-to/install-spark.md).
For serving a model see [How to serve a model on Spark](../how-to/serve-a-model-on-spark.md).

The laptop CLI never holds Docker authority. Engines run on an
internal managed bridge; `ps` reports only active model processes
without printing that address.

Maintenance uses SSH only. `upgrade` accepts the install artifact environment
variables; `rollback` and `cert rotate` remain usable when HTTPS is unavailable.
All three require exactly one of `--dry-run`/`SY_SPARK_DRY_RUN` or
`--yes`/`SY_SPARK_YES`, and support `--json`/`SY_SPARK_JSON` plus
`SY_SPARK_CONFIG_DIR`. Rollback JSON uses `sy.spark.maintenance/v1`; certificate
rotation uses `sy.spark.certificate-rotation/v1`. Exit codes are `0` success,
`2` local usage/artifact failure, `3` remote compatibility or safety rejection,
and `4` SSH/TLS/agent reachability failure.

The authenticated `GET /api/sy.spark/v1/metrics` endpoint returns bounded
Prometheus text without prompt, generated text, credentials, operation IDs,
commits, or client IDs as labels. It requires an admin token. Engine logs remain
bounded, cursored, redacted, and protected by `logs:read`.

## Synopsis

```text
sy spark <host> install --dry-run --json
sy spark <host> install --yes --release-manifest <SHA256SUMS> --release-signature <sig> --release-public-key <pub>
sy spark <host> upgrade --dry-run --json
sy spark <host> rollback --dry-run --json
sy spark <host> cert rotate [--ca] --dry-run --json
sy spark <host> status --json
sy spark <host> doctor --json
sy spark <host> serve <model> [--name <instance>] [--detach] [--dry-run] [--json]
sy spark <host> launch <codex|claude|opencode> [--model <model>] [--config] [--restore] [-y] [-- <agent-args>...]
sy spark <host> ls [--json]
sy spark <host> ps [--json]
sy spark <host> logs <instance> [--limit N]
sy spark <host> stop <instance>
sy spark <host> download <repo> --revision <sha> --alias <name>
sy spark <host> client-config <name> --client <codex|claude-code>
```

## Description

`ls` is the everyday inventory of verified models available to run. Its default
columns are `NAME`, `ID`, `SIZE`, and `MODIFIED`. `ps` is the active lifecycle
view: it omits absent and failed historical records and prints `NAME`, `MODEL`,
`ENGINE`, `CONTEXT`, and `STATE`. `ps --json` contains the same active set. Use
`show`, `logs`, and `ls --json` when complete immutable identity, provenance, or
diagnostic state is required.

Before an engine lifecycle is authorised, the agent requires one
fresh executor-owned snapshot and checks aggregate cold-start
memory, live `MemAvailable`, full-memory PSI, swap-in activity,
disk reserve, immutable model provenance, and the single high-memory
start lease. Stop does not take that lease: reducing memory must remain
available while a start or startup reconciliation is active.

The root-owned `/etc/sy/spark/engines/*.toml` catalog is the serving policy. It
owns each engine image and digest, entrypoint, bounded arguments, environment,
mounts, network, UID, resource envelope, health probe, public route allowlist,
sampling defaults, and finite profiles. A signed `models.toml` entry may select
an `engine_profile` explicitly; otherwise selection falls back to the model's
`config.json` `model_type` and then the engine default. Rust owns schema
validation and security invariants only. It does not embed model IDs, image
versions, digests, tuning values, or per-model commands.

Auxiliary artifact roles are open lowercase identifiers declared by the model
catalog. Each engine configuration must either bind a role to an exact confined
file argument or list the role under `ignored_roles`; an unbound role fails
planning. Profiles can therefore add projectors, MTP drafts, adapters, or future
engine inputs without adding model-aware Rust branches.

`serve` accepts only a verified model reference and optional instance name. The
HTTP schema rejects unknown fields, so callers cannot inject an image,
entrypoint, mount, network, or argv. A model supported by the configured vLLM
version can be downloaded and served without rebuilding sy. Unsupported models
fail at bounded startup/semantic validation; sy never substitutes another
engine.

After health check and an exact model-identity completion probe,
the agent publishes:

- OpenAI-compatible routes at
  `https://<spark>:9843/openai/<instance>/v1`
- Anthropic Messages routes under
  `/anthropic/<instance>/v1`

The allowlist exposes authenticated models, completions,
protocol-native chat completions, Responses, Messages, and token
count with bounded SSE and client-side tool continuation. Engine-native health,
metrics, tokenizer, debug, admin, and addresses stay private; the separate
control-plane metrics endpoint requires an admin token.

The gateway preserves Ornith's parsed reasoning as a distinct channel. OpenAI
Chat streams `reasoning_content`; Responses streams reasoning summary item
events and includes the same item in non-stream documents; Anthropic streams a
thinking block and accepts sy's integrity-checked block back in later turns.
The sy-owned Codex catalog advertises reasoning-summary support so Codex sends
the summary request and renders these native Responses events instead of
discarding them as unsupported model output.
Anthropic `display: "omitted"` emits the block and signature without thinking
deltas, while `type: "disabled"` disables thinking in the Ornith template.

Missing sampling values come from the selected profile in `engine.toml` and are
applied consistently to OpenAI Chat, Responses, and Anthropic Messages. Explicit
client values are preserved. Runtime workarounds are profile arguments in the
same file. The current Qwen profile selects eager execution because the compiled
Qwen 3.5 GEMM path can terminate vLLM on GB10; removing that workaround requires
only a configuration edit after the upstream path is proven stable.

Warming or recovering generations return protocol-native `503`
with `Retry-After` and never inherit a stale route.

## Admission reserves

| Reserve | Value |
|---------|-------|
| System reserve | 8 GiB |
| Emergency floor | 8 GiB |
| Disk reserve | 100 GiB |

Missing or stale telemetry fails closed. The root executor samples
pressure independently and can suppress restart before an emergency
victim is stopped; it never selects unlabeled work.

## Engine profiles and modalities

- Ornith accepts bounded inline JPEG, PNG, or WebP through OpenAI
  Responses and Anthropic Messages. The adapters validate media
  type, file magic, decoded bytes, image count, and dimensions
  before contacting vLLM. Remote URLs, local paths, traversal,
  unsupported media, and images sent to text-only instances are
  rejected.
- Model types needing finite parser arguments are declared as profiles in
  `engine.toml`. Unknown model types use the declared default profile. Profiles
  cannot override the image, executable, mounts, network, UID, or arbitrary
  command line.
- The `qwen3.8-mtp` llama.cpp profile preserves reasoning and selects
  `draft-mtp` speculation with a verified Q4_0 MTP artifact. It imposes no
  reasoning token guard.

## `client-config`

`--client codex` prints a projection for Codex
`wire_api = "responses"`. `--client claude-code` prints a Claude
Code 2.1.241 projection. Both name the protected token and CA
environment variables without reading, printing, or persisting the
token.

## `launch`

`launch` runs a registered coding-agent executable on the workstation and
routes its model calls to one exact managed Spark instance. A healthy matching
instance is reused; otherwise the configuration-driven `serve` operation is
followed to healthy. The launcher never guesses or downloads a missing model.

`--model` selects a verified model ID, canonical identity, repository, exact
alias, or exact instance name shown by `sy spark <host> ps`. A stopped selected
instance is re-served under that same managed name. Without `--model`, the saved
host/integration selection is reused; an interactive terminal can select from
installed models. `--config` writes only launch-owned state/config and exits.
`--dry-run` performs no local or remote mutation. `--json` is valid with either
of those non-agent modes. `--restore` removes only sy-owned Codex
profile/catalog files. `-y` permits the fixed Claude or OpenCode installer when
the executable is absent. Only arguments after `--` are forwarded, without a
shell.

State is serialized with a local lock. Metadata stores only the token ID; the
bearer is held separately in a mode-0600 credential file. The child receives an
inference-only token and pinned CA, never the administrator credential. Claude
uses the native Anthropic route, Codex uses a sy-owned Responses profile and
catalog, and OpenCode uses process-local `OPENCODE_CONFIG_CONTENT`.

## Examples

```bash
sy spark dgx-spark serve ornith-1.5:9b --dry-run --json
sy spark dgx-spark launch codex --model ornith-1.5:9b
sy spark dgx-spark launch claude --model ornith-1.5:9b -- --permission-mode plan
sy spark dgx-spark launch opencode --model ornith-1.5:9b
sy spark dgx-spark serve ornith-1.5:9b --name ornith
sy spark dgx-spark ps --json
sy spark dgx-spark logs ornith --limit 100
sy spark dgx-spark stop ornith
```

## See also

- [How to install the Spark agent](../how-to/install-spark.md)
- [How to serve a model on Spark](../how-to/serve-a-model-on-spark.md)
- [CLI: `sy spark`](cli.md#sy-spark)
- [Glossary: spark](glossary.md#spark)
