<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to serve a model on Spark

## Goal

Ask a Spark host to run a verified model with its configured engine, wait until the instance is
healthy, and confirm the OpenAI-compatible gateway path answers.

## Prerequisites

- A DGX Spark on the network with the `sy spark` agent and executor
  already installed and authenticated. If they are not, follow
  [How to install the Spark agent](install-spark.md) first.
- `sy` on your laptop with Spark client credentials configured for
  that host. Substitute `dgx-spark` below for the host name you use.
- Enough free memory and disk on the Spark for the configured engine envelope. Missing
  or stale telemetry fails closed; the agent will refuse the serve
  rather than overcommit.

## Steps

1. Preview admission without starting anything. The report includes
   the 8 GiB system reserve, 8 GiB emergency floor, and 100 GiB disk
   reserve:

   ```bash
   sy spark dgx-spark serve ornith-1.5:9b --dry-run --json
   ```

   If the dry-run is denied, free memory or disk on the Spark and
   retry. Missing telemetry fails closed: the agent refuses
   rather than guessing.

2. Start the verified model. The agent selects the single engine configured at
   `/etc/sy/spark/engine.toml`; there is no image, recipe, argv, or unsafe-override
   flag on the request:

   ```bash
   sy spark dgx-spark serve ornith-1.5:9b --name ornith
   ```

3. Watch desired versus observed state. `ps` does not print the
   internal bridge address:

   ```bash
   sy spark dgx-spark ps --json
   ```

4. To add another model, download an immutable revision and serve its alias:

   ```bash
   sy spark dgx-spark download owner/model --revision <commit> --alias model:tag
   sy spark dgx-spark serve model:tag --name model
   ```

   Models supported by the installed vLLM version need no code change. If a
   model requires a finite parser or capability, add a `model_type` profile to
   the root-owned engine configuration and restart the two sy services. Never
   place image names, model IDs, or launch arguments in Rust.

5. Print a client config for Codex or Claude Code. The output names
   the token and CA environment variables; it does not print or
   persist the token:

   ```bash
   sy spark dgx-spark client-config ornith --client codex
   sy spark dgx-spark client-config ornith --client claude-code
   ```

6. Launch a local coding agent against the healthy model. Arguments after `--`
   go directly to that client as exact argv tokens:

   ```bash
   sy spark dgx-spark launch codex --model ornith-1.5:9b
   sy spark dgx-spark launch claude --model ornith-1.5:9b -- --permission-mode plan
   sy spark dgx-spark launch opencode --model ornith-1.5:9b
   ```

   Use `--dry-run --json` to inspect model/instance selection without changing
   local or remote state, or `--config --json` to provision configuration and
   the inference-only token without starting the agent. A missing model is not
   downloaded implicitly; follow the printed immutable `download --revision`
   remediation. `launch` does not edit the primary Claude/OpenCode config or
   the main Codex config.

7. Stop when you are done. An already-absent instance is an idempotent success:

   ```bash
   sy spark dgx-spark stop ornith
   ```

## Result

`ps --json` shows the instance you named. After health check and
model-identity probe pass, OpenAI-compatible routes are at
`https://<spark>:9843/openai/<instance>/v1` and Anthropic Messages
routes are under `/anthropic/<instance>/v1`. Warming or recovering
generations return protocol-native `503` with `Retry-After`.

For Ornith, reasoning streams independently from final answer text as OpenAI
`reasoning_content`, Responses reasoning-summary events, or Anthropic thinking
blocks. `sy spark ... launch codex` advertises that capability in its private
model catalog, so Codex renders the Responses reasoning stream. Requests that
omit sampling settings receive Ornith's precise-coding defaults; explicit
client settings remain unchanged.

## See also

- [How to install the Spark agent](install-spark.md)
- [Spark reference](../reference/spark.md)
- [CLI: `sy spark`](../reference/cli.md#sy-spark)
