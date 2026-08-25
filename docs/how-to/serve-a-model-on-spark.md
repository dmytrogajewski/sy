<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to serve a model on Spark

## Goal

Ask a Spark host to run a signed recipe, wait until the instance is
healthy, and confirm the OpenAI-compatible gateway path answers.

## Prerequisites

- A DGX Spark on the network with the `sy spark` agent and executor
  already installed and authenticated. If they are not, follow
  [How to install the Spark agent](install-spark.md) first.
- `sy` on your laptop with Spark client credentials configured for
  that host. Substitute `dgx-spark` below for the host name you use.
- Enough free memory and disk on the Spark for the recipe. Missing
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
   retry. Do not pass `--allow-unverified` to skip a denial you do
   not understand. Missing telemetry fails closed: the agent refuses
   rather than guessing.

2. Serve a signed fixture first if you want to prove the Docker
   lifecycle without loading weights or using the GPU. The fixture
   recipe is digest-pinned ARM64 HTTP:

   ```bash
   sy spark dgx-spark serve ornith-1.5:9b \
     --recipe spark-fixture-http-echo-1.0.0 \
     --name fixture
   ```

3. Watch desired versus observed state. `ps` does not print the
   internal bridge address:

   ```bash
   sy spark dgx-spark ps --json
   ```

4. Optionally select from installed locally verified engines. This does not
   measure speed, download, pull, convert, or start an engine:

   ```bash
   sy spark dgx-spark tune ornith-1.5:9b --objective agent --json
   ```

   When you are ready for a real engine, omit `--recipe` to use the exact tuned
   winner or the visible verified vLLM fallback, or pass a named recipe. Example
   embedding recipe:

   ```bash
   sy spark dgx-spark download Qwen/Qwen3-Embedding-0.6B \
     --revision 97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3 \
     --alias qwen3-embedding:0.6b
   sy spark dgx-spark serve qwen3-embedding:0.6b \
     --recipe qwen3-embedding-0.6b-vllm-0.19.1 \
     --name embeddings \
     --allow-unverified
   ```

   `--allow-unverified` is for recipes that are not yet in the
   signed set. Prefer a signed recipe when one exists.

5. Print a client config for Codex or Claude Code. The output names
   the token and CA environment variables; it does not print or
   persist the token:

   ```bash
   sy spark dgx-spark client-config ornith --client codex
   sy spark dgx-spark client-config ornith --client claude-code
   ```

6. Stop when you are done. An already-absent instance is an idempotent success:

   ```bash
   sy spark dgx-spark stop fixture
   ```

## Result

`ps --json` shows the instance you named. After health check and
model-identity probe pass, OpenAI-compatible routes are at
`https://<spark>:9843/openai/<instance>/v1` and Anthropic Messages
routes are under `/anthropic/<instance>/v1`. Warming or recovering
generations return protocol-native `503` with `Retry-After`.

## See also

- [How to install the Spark agent](install-spark.md)
- [Spark reference](../reference/spark.md)
- [CLI: `sy spark`](../reference/cli.md#sy-spark)
