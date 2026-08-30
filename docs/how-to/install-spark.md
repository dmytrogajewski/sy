<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to install the Spark agent

## Goal

Install the signed ARM64 Spark agent and root executor on a DGX Spark
over OpenSSH, then confirm the authenticated HTTPS status plane
answers from your laptop.

## Prerequisites

- An OpenSSH host alias that already works, for example `dgx-spark`.
  `ssh dgx-spark true` must succeed with the keys, agent, or token
  you already use. `sy` does not accept passwords, keys, or tokens as
  arguments and does not store them.
- A signed ARM64 `sy` release bundle for the Spark: `sy-aarch64`, the three
  separate catalog TOMLs, `SHA256SUMS`, its minisign signature, and the pinned
  minisign public key. `--yes` refuses to
  run without `--release-signature` and `--release-public-key`.
- `sy` on your laptop `$PATH`.

This how-to installs the two Spark-host units
[`sy-spark-agent.service`](../../configs/systemd/system/sy-spark-agent.service)
(unprivileged `User=sy-spark`, HTTPS on port 9843) and
[`sy-spark-executor.service`](../../configs/systemd/system/sy-spark-executor.service)
(root, Docker, private network). The network-facing agent never gets
a Docker socket.

## Steps

1. Print the non-mutating plan. Dry-run uploads one content-addressed
   probe, invokes only `spark bootstrap inspect`, verifies the hash,
   and removes that exact path:

   ```bash
   sy spark dgx-spark install --dry-run --json
   ```

   Substitute your OpenSSH alias for `dgx-spark`. Review the
   `sy.spark.install-manifest/v1` document before you approve.

2. Apply the reviewed plan. This uploads only the signed
   content-addressed release and invokes the fixed bootstrap
   activation entrypoint. No arbitrary remote command is accepted:

   ```bash
   scripts/package-spark-release.sh target/aarch64-unknown-linux-gnu/release/sy release
   minisign -Sm release/SHA256SUMS -s sy-release.key
   sy spark dgx-spark install --yes \
     --probe release/sy-aarch64 \
     --release-manifest release/SHA256SUMS \
     --release-signature release/SHA256SUMS.minisig \
     --release-public-key sy-release.pub
   ```

   Point those two flags at the signature and public key that match
   the ARM64 release you trust. Flags override `SY_SPARK_RELEASE_SIGNATURE`
   and `SY_SPARK_RELEASE_PUBLIC_KEY`.

3. Confirm the agent and executor over pinned HTTPS:

   ```bash
   sy spark dgx-spark status --json
   sy spark dgx-spark doctor --json
   ```

## Result

`status --json` reports the agent listening on the configured port
(default 9843) and the executor healthy. `doctor --json` exits `0`.
You can now [serve a model](serve-a-model-on-spark.md).

## Upgrade or recover the control plane

Preview and approve the same signed ARM64 artifact transaction used at first
install:

```bash
sy spark dgx-spark upgrade --dry-run --json
sy spark dgx-spark upgrade --yes \
  --probe release/sy-aarch64 \
  --release-manifest release/SHA256SUMS \
  --release-signature release/SHA256SUMS.minisig \
  --release-public-key sy-release.pub \
  --json
```

Upgrade verifies a database backup, N/N-1 schema compatibility, active engine identities,
and the protected DGX fingerprint. It changes only the control-plane release and
keeps healthy engine containers running. Failed semantic health requests
automatic rollback.

Use the SSH-only recovery path even when HTTPS is unavailable:

```bash
sy spark dgx-spark rollback --dry-run --json
sy spark dgx-spark rollback --yes --json
sy spark dgx-spark cert rotate --dry-run --json
sy spark dgx-spark cert rotate --yes --json
```

Add `cert rotate --ca --yes` only when replacing the local CA. The new public CA
returns through SSH and the client atomically re-pins it; private keys remain on
Spark. Docker restart and host reboot are not part of these commands and remain
`not_run` unless the operator separately schedules them.

Exit `2` is local configuration or usage. Exit `4` is OpenSSH, SFTP,
TLS pin mismatch, or authentication failure — fix the SSH alias or
the pinned CA, not the Spark units.

## See also

- [CLI: `sy spark`](../reference/cli.md#sy-spark)
- [Spark reference](../reference/spark.md)
- [How to serve a model on Spark](serve-a-model-on-spark.md)
