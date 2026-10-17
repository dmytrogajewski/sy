# Journey: install the agent runner without the desktop bundle

**Actor:** Fedora user who wants agent sessions and policy controls on a minimal system.
**Goal:** install `sy` and `sy-system-agent`, then run `sy agt`, `sy policy`, and `sy approve` without `sy-desktop`.

1. Install matching RPMs for the host Fedora release.
2. Start `sy-agentd.service` in the user manager and inspect its status.
3. Run `sy agt diag --json`; the command reaches the agent binary and reports configured agents.
4. Use `sy policy lint --json` against a policy directory and manage sessions through the shared launcher.

## Failure paths

- When the feature package is absent, the launcher names `sy-system-agent` and exits 69.
- An unavailable daemon returns the agent CLI's documented error instead of invoking desktop code.
- Agent subprocesses re-exec through the standalone feature binary with their sandbox arguments intact.
