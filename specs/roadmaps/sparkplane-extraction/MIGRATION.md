# Migration from sy to Sparkplane

The live migration completed on 2026-09-21 using signed Sparkplane v0.1.4.
The appliance and both CLI entrypoints are verified; the matched decode median
is 33.804 tokens/s versus 33.808 before migration, with 262144-token context.
Do not run fresh install over another existing sy appliance: a namespace
migration is not an ordinary upgrade. The procedure below remains the migration
runbook, not an instruction to repeat the completed transaction.

## Client configuration import

The following command previews an offline import, without SSH, network traffic,
appliance changes or credential output:

```sh
sparkplane local migrate-client --dry-run --json
```

Once the appliance transition has been verified, repeat with `--yes` instead
of `--dry-run`. Defaults are `$XDG_CONFIG_HOME/sy` and
`$XDG_CONFIG_HOME/sparkplane` (or `$HOME/.config/` when unset). Override them
with absolute `--source` and `--destination` paths when necessary.

Only `spark.toml`, optional `spark-launch.toml`, the `spark/` tree (CA certificates
and retained catalog assets), and `credentials/spark/` are imported. Retained
local catalog assets do not replace the appliance's signed catalog. The import
preserves credential bytes, converts the
typed launch-state schema, refuses symlinks and existing destinations, and
atomically publishes private files. Source files and unrelated sy settings
remain untouched. Generated coding-client files outside this directory are not
modified by this command. Do not delete them before ownership-aware migration
and launch verification have succeeded.

## Appliance transition gate

The appliance build now includes the concrete, journaled bootstrap runner.
It fences new traffic, drains established connections, measures the existing
engine, stops both state writers, stages an integrity-checked SQLite backup
(including WAL), and relocates data/cache directories without copying weights.
Service-account renames preserve numeric UID/GID. TLS keys, token verifiers,
credentials, model identities and historical audit rows are retained.
The executor's emergency replay journal is separately validated against its
imported database evidence, converted only in its typed schema fields, and
exchanged with an exact original-file backup. Recovery restores the original
bytes and retains any failed-generation journal separately. Unknown schemas,
unimported/mismatched evidence and symlinks reject preflight.

Preflight rejects model/engine catalog changes beyond owned namespaces,
unresolved operations, missing successful serve history, unhealthy instances,
ownership conflicts, cross-filesystem moves and insufficient disk reserve.
Source catalog and model/instance inventory digests are rechecked after drain
and after stopping state writers. The current finite qualification contract
supports running vLLM instances with tool calling; other active engines must
not be silently migrated through an untested qualification path.

Stopped historical instances keep their original engine fingerprints, context
settings and suppression/quarantine flags. Only running instances must match the
current signed policy and have
their active compile-cache keys relocated. Historical rows still undergo model,
artifact and identity validation; they are never restarted by migration.

Managed Docker policies `no` and `unless-stopped` honor the runner's explicit
exact-ID stop. Other policies are rejected, and lifecycle operations recheck
the policy along with container ownership. No restart-policy rewrite is needed.

Before commit, qualification checks pinned HTTPS readiness, full configured
context, separate reasoning content, tool continuation, complete streams,
concurrent requests, cancellation drainage and three matched decode samples.
Median throughput may not regress more than 5%. Exact old containers and their
empty, identity-verified internal network are removed before the commit marker.
Only then can external traffic reopen. These are automated gates, not evidence
that live acceptance has already occurred.

A transition manifest binds the new release authority to one host identity,
one exact signed release inventory digest, and an expiration. The installed
legacy authority must sign it. Unlock the signing key locally or use a trusted
offline signing station; never send a passphrase in chat or commit a private key.

The optimized engine image and runtime settings must remain unchanged apart
from namespaces. Live acceptance requires full context, tools, streaming,
cancellation, concurrency and matched performance measurements. Keep the legacy
deployment serving until all release and migration gates are ready.

## Operator authorization and cutover

1. Provision the dedicated encrypted Sparkplane release key and the protected
   release-environment secrets using the standalone developer guide's
   “Provision the release authority” section. Complete the signed release;
   unsigned workflow artifacts are build evidence, not deployment authority.
2. Obtain the host identity: SHA-256 of the trimmed 32-character contents of
   `/etc/machine-id` on Spark, with no trailing newline included in the hash.
   The release identity is SHA-256 of the exact inner appliance `SHA256SUMS`.
3. Prepare this transition, using real values and a finite future expiry:

   ```json
   {
     "schema": "sparkplane.trust-transition/v1",
     "host_identity": "<64 lowercase hexadecimal characters>",
     "release_sha256": "<SHA-256 of the inner appliance SHA256SUMS>",
     "new_public_key": "<new Minisign base64 public-key line>",
     "expires_at_unix_seconds": 0
   }
   ```

   Sign its exact bytes with the installed legacy authority. Run this in your
   terminal, entering the key-unlock password only into Minisign's prompt:

   ```sh
   minisign -Sm trust-transition.json \
     -s "$HOME/.config/sy/spark-release-signing/release.key"
   ```

4. Verify the transition against the established legacy public key, then verify
   the release inventory and all payload hashes against the approved new key.
   Transfer the bundle and signed transition over the existing SSH connection.
   Before any root execution, stage the verified executable under a root-owned,
   non-shared-writable directory and independently confirm its exact signed
   digest there. Never execute an unverified or user-writable upload as root.
5. On Spark, use that exact approved appliance executable for both commands:

   ```sh
   sparkplane bootstrap migrate-appliance \
     --bundle /absolute/path/to/verified-bundle \
     --transition /absolute/path/to/trust-transition.json \
     --transition-signature /absolute/path/to/trust-transition.json.minisig \
     --dry-run --json
   ```

   Review the result; repeat with `--yes` instead of `--dry-run`. This is a local
   root bootstrap operation, not a remote arbitrary-command API. The executing
   binary must itself match the signed appliance payload. Model reload occurs
   only after baseline qualification and the durable snapshot are complete.
6. Once live acceptance succeeds, import the workstation configuration, install
   the exact signed client pin through sy, and verify direct `sparkplane` and
   bridged `sy spark` status/launch behavior. Existing generated coding-client
   files remain protected by ownership receipts; resolve conflicts explicitly.

## Interruption recovery

The private transaction and evidence live under
`/var/lib/sparkplane-migration/active`. Every host step is recorded before it
starts. Persistent systemd guards prevent either namespace from starting after
a reboot until the recovery command restores the traffic fence and volatile
permit. Docker restart and host reboot are not migration actions.

Use the original approved executable, on the original host, as root:

```sh
sparkplane bootstrap migrate-appliance --recover --json
```

`--recover` is valid only before commit. It stops and cleans up exact new
containers, reverses published files/account/data/cache moves, restores the
original database files, and verifies the previous service before reopening
traffic. Evidence is retained in a private `recovered-<UUID>` directory.

If recovery itself needed a managed replacement generation, a newer signed
release can finish the health-check phase using a separate installed-authority
approval. This is accepted only after the journal confirms rollback has restored
all files, accounts, caches and database state. The approval schema is
`sparkplane.recovery-approval/v1`; it binds `host`, `record_sha256` (the exact
unchanged `active/record.json` bytes), `executable_sha256` (the new verified
ARM64 executable), finite `expires_at_unix_seconds`, and `replacements` (the
complete exact legacy container identities, with explicitly advanced generations
and container IDs). Every other image, engine and model identity must match the
original plan. Sign it with the installed authority, then run:

```sh
sparkplane bootstrap migrate-appliance --recover \
  --recovery-approval /absolute/path/to/recovery-approval.json \
  --recovery-signature /absolute/path/to/recovery-approval.json.minisig --json
```

The receipt and signature are retained in the transaction; the original record
and database are never rewritten to satisfy recovery. Normal pinned-HTTPS health,
generation and full-context checks must succeed before traffic reopens. The
handoff cannot authorize a forward migration or post-commit rollback. Ordinary
recovery still requires the original executable. Startup guards allow 1900
seconds for both agents so cold model loading does not trigger the default
90-second systemd service deadline; guards are removed after verified recovery.

If the transaction committed but reopening traffic was interrupted, use:

```sh
sparkplane bootstrap migrate-appliance --resume --json
```

`--resume` verifies the committed service and completes activation; it never
restores an old database over accepted work. Neither recovery mode accepts a
replacement bundle or transition. Do not edit the journal or delete a fence to
bypass a failed check. Qualification bodies and credentials are not stored in
performance evidence; before/after evidence contains only instance IDs and
decode rates.
