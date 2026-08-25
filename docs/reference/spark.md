<!-- Template source: Good Docs Project reference template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/reference. Diátaxis quadrant: reference. -->

# Spark reference

`sy spark <host>` drives a Spark appliance from your laptop. Use this
page for admission numbers, gateway paths, and recipe rules. For
install steps see [How to install the Spark agent](../how-to/install-spark.md).
For serving a model see [How to serve a model on Spark](../how-to/serve-a-model-on-spark.md).

The laptop CLI never holds Docker authority. Engines run on an
internal managed bridge; `ps` reports desired versus observed
state without printing that address.

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
sy spark <host> install --yes --release-signature <sig> --release-public-key <pub>
sy spark <host> upgrade --dry-run --json
sy spark <host> rollback --dry-run --json
sy spark <host> cert rotate [--ca] --dry-run --json
sy spark <host> status --json
sy spark <host> doctor --json
sy spark <host> bench <model> [--recipe <id>] [--dry-run] [--json]
sy spark <host> tune <model> [--objective <agent|interactive|long-context|retrieval>] [--detach] [--dry-run] [--json]
sy spark <host> serve <model> [--recipe <id>] [--name <instance>] [--dry-run] [--json] [--allow-unverified]
sy spark <host> ps [--json]
sy spark <host> logs <instance> [--limit N]
sy spark <host> stop <instance>
sy spark <host> download <repo> --revision <sha> --alias <name>
sy spark <host> client-config <name> --client <codex|claude-code>
```

## Description

Before an engine lifecycle is authorised, the agent requires one
fresh executor-owned snapshot and checks aggregate cold-start
memory, live `MemAvailable`, full-memory PSI, swap-in activity,
disk reserve, recipe compatibility, and the single high-memory
transition lease.

Capabilities come only from the selected signed recipe. The named
fixture recipe is a signed, digest-pinned ARM64 HTTP engine used to
verify the Docker lifecycle without loading weights or using the
GPU. Omitting `--recipe` uses an exact tuned winner when valid and otherwise
reports and uses verified vLLM as the fallback.

`bench` evaluates one installed exact recipe; `tune` evaluates the finite set of
installed locally verified recipes. Both use functional gates for identity,
capabilities, semantic evidence, safety, isolation, health, and durability. They
do not measure speed or implicitly download, pull, convert, or launch an engine.
The persisted winner is keyed by the complete fingerprint and objective; drift
invalidates it and restores the visible verified vLLM fallback.

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

## Recipes and modalities

- Ornith accepts bounded inline JPEG, PNG, or WebP through OpenAI
  Responses and Anthropic Messages. The adapters validate media
  type, file magic, decoded bytes, image count, and dimensions
  before contacting vLLM. Remote URLs, local paths, traversal,
  unsupported media, and images sent to text-only instances are
  rejected.
- `Qwen/Qwen3-Embedding-0.6B` has an embedding-only recipe at
  `POST /openai/<instance>/v1/embeddings` (1024-dim unit-normalised
  float vectors). It does not expose generation routes. Input
  order, dimensions, usage, and public model identity are checked
  by the gateway.

## `client-config`

`--client codex` prints a projection for Codex
`wire_api = "responses"`. `--client claude-code` prints a Claude
Code 2.1.241 projection. Both name the protected token and CA
environment variables without reading, printing, or persisting the
token.

## Examples

```bash
sy spark dgx-spark serve ornith-1.5:9b --dry-run --json
sy spark dgx-spark bench ornith-1.5:9b --dry-run --json
sy spark dgx-spark tune ornith-1.5:9b --objective agent --json
sy spark dgx-spark serve ornith-1.5:9b --recipe spark-fixture-http-echo-1.0.0 --name fixture
sy spark dgx-spark ps --json
sy spark dgx-spark logs fixture --limit 100
sy spark dgx-spark stop fixture
```

## See also

- [How to install the Spark agent](../how-to/install-spark.md)
- [How to serve a model on Spark](../how-to/serve-a-model-on-spark.md)
- [CLI: `sy spark`](cli.md#sy-spark)
- [Glossary: spark](glossary.md#spark)
