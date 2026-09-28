# Sparkplane extraction and bridge contracts (sy integration)

Sparkplane is the authoritative owner of appliance code, assets, engines,
models, qualification, release packaging and operator documentation. sy owns
only installation of a pinned client and command forwarding. Neither project
uses a source path, git dependency or workspace member from the other.

## Stable surfaces

`sparkplane HOST COMMAND` accepts the same command tail as `sy spark HOST COMMAND`.
The bridge preserves arguments (including non-UTF-8 Unix arguments), inherited
stdio, terminal ownership, signals and child exit status through `exec`.
It verifies the managed executable and `sparkplane.bridge/v1` protocol before
execution; no command-time download or PATH-based executable substitution occurs.
Legacy `SY_SPARK_*` environment variables translate at the sy boundary only;
explicit `SPARKPLANE_*` values take precedence.

Client installation uses `$XDG_DATA_HOME/sparkplane/client/releases/` and an
atomic `current` link. Appliance releases use `/opt/sparkplane/releases/`.
Client configuration belongs under `$XDG_CONFIG_HOME/sparkplane/`; appliance
configuration, data and sockets use `/etc/sparkplane/`, `/var/lib/sparkplane/`
and `/run/sparkplane/`. The service account retains its numeric UID/GID when
renamed, so large model trees do not require recursive ownership changes.

OpenAI/Anthropic inference URLs, aliases, checkpoint identities, certificate
material and existing credentials survive migration. New tokens use a
`sparkplane_` prefix; existing `sy_` tokens remain valid opaque credentials.
The new control API and JSON schema identifiers are a documented namespace break.

## State transition safety

Migration requires the exact signed trust transition and a verified new release.
Read-only preflight rejects unresolved operations, incompatible database versions,
quarantined/suppressed instances, destination conflicts and cross-filesystem cache
moves before stopping services. Namespace conversion is typed: active model and
instance metadata changes, while model/checkpoint identities, audit rows,
operation history and token verifier bytes do not.

SQLite snapshots use its online backup API, not copying a database file while
WAL writers are active. Tests must cover WAL contents, failed stages and recovery.
Never rewrite a signed manifest or content-addressed patch to change its name.

The live cutover is a separate acceptance gate: drain traffic, journal each
step, preserve rollback evidence and perform one planned reload of the same
optimized engine. Do not restart Docker, reboot the host, re-download model
weights or relax admission/security policies. A snapshot cannot be restored
over new work accepted after commit.

## Optimized vLLM acceptance

The Qwen 3.8 Flash Next profile retains 262,144-token context, MTP, prefix caching,
the draft vocabulary, PLE mmap, CUDA graphs, KV cache and sampling settings.
Compare the exact pinned image and runtime arguments before and after migration.
Run streaming, reasoning/tool continuation, cancellation, concurrency and
maximum-context probes. Repeat matched throughput measurements and investigate
an unexplained regression greater than 5% before accepting cutover.

## Release operations

The `CI` workflow checks both build features. The `Release` workflow builds
x86-64/ARM64 clients and the ARM64 appliance. Tagging `vVERSION` invokes the
protected signing/publishing job; manual workflow dispatch builds without
publishing. The artifact inventory is signed, and the appliance includes Cargo
metadata, resolved features and embedded cargo-auditable dependency records.

Provision `SPARKPLANE_MINISIGN_SECRET_KEY` and `SPARKPLANE_MINISIGN_PASSWORD`
only in the protected release environment. Export the public key separately for
operators and sy's release pins. Branch protection must require the `verify`
check. Release credentials must not be exposed to PR workflows.
