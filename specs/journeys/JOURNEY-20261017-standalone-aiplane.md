# Journey: install AI separately from knowledge

**Actor:** Fedora user adding local AI to a base sy installation.

**Goal:** install `sy-system-aiplane`, start its user service, and run an AI workload without installing the desktop or knowledge service. A desktop user can add knowledge later without starting a second inference supervisor.

## Happy path

1. Install matching `sy` and `sy-system-aiplane` RPMs. The AI RPM owns `/usr/libexec/sy/sy-system-aiplane`, `sy-aiplane.service`, and its workload configuration.
2. Start `sy-aiplane.service`. It owns `$XDG_RUNTIME_DIR/sy-aiplane.sock`, the inference worker supervisor, and `$XDG_RUNTIME_DIR/sy/aiplane/metrics.sock`.
3. Run `sy aiplane status --json`, then `sy aiplane run --workload embed --input '{"kind":"text","text":"hello"}' --json`. The command reaches the service over IPC.
4. If `sy-desktop` is installed, start `sy-knowledge.service`. Indexing and query embedding reach the AI socket. Knowledge writes its own metrics socket and reports the AI worker health from the AI runtime snapshot.
5. Install `sy-system-aiplane-ryzenai` and an external AMD runtime on supported hardware. After a Fedora upgrade, run the packaged NPU check and confirm the embed workload uses VitisAI.

## Failure paths

- Starting a second AI daemon against an active AI socket fails and leaves the first listener reachable.
- A legacy knowledge daemon that still advertises AI methods blocks AI daemon startup; restart knowledge before starting the new service.
- If the dedicated service is down, the CLI reports that service and knowledge pauses indexing. It never starts a second local NPU session.
- Missing proprietary runtime or model artifacts leave the CPU capable package installable; the upgrade check reports the specific incompatibility.

## Acceptance

- Spawned daemon tests cover dedicated IPC, status, duplicate startup, shutdown, and daemon-down CLI behavior.
- A knowledge query embedding test uses the dedicated IPC path when no local supervisor exists.
- `make lint`, `make test`, `make lint-spark`, and `make test-spark` pass.
- Fedora 43 and 44 RPMs build and contain the dedicated service and executable.
- NPU execution on Fedora 44 remains a hardware qualification gate on a machine with `/dev/accel/accel0` and the matching AMD runtime.
