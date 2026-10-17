# Journey: install AI workloads as an optional system feature

**Actor:** Fedora user installing sy on a host that may have an AMD NPU.
**Goal:** install `sy-system-aiplane` separately from the desktop and run workload commands.

1. Install matching `sy` and `sy-system-aiplane` RPMs.
2. Run `sy aiplane list --json` to inspect available workloads and prepared models.
3. Run `sy aiplane status --json` to inspect daemon and backend state.
4. On an NPU host, install matching Ryzen AI runtime packages and run the upgrade check, then execute `sy aiplane run --workload embed` against the daemon.

## Failure paths

- If the feature RPM is missing, the launcher names `sy-system-aiplane` and exits 69.
- If the AMD runtime is absent or incompatible, the upgrade check reports the missing path or library before a workload runs.
- A failed workload returns a structured error rather than a partial vector.
