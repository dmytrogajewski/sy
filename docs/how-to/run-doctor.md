<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to run sy doctor

## Goal

Run the health probes and read the exit code so you know whether
the install is healthy, drifted, or failed.

## Prerequisites

- `sy` is on `$PATH`.
- For a full pass, `sy.target` should be running. Doctor still runs
  if some units are down; those probes fail instead of hanging.

## Steps

1. Run the human-readable pass:

   ```bash
   sy doctor
   echo $?
   ```

2. Run the machine-readable pass. The document is
   `sy.doctor/v1` on stdout:

   ```bash
   sy doctor --json
   ```

3. If you are chasing one plane, restrict the check name prefix:

   ```bash
   sy doctor --only=ipc.
   sy doctor --only=kernel.
   ```

## Result

The command prints the probe list. `echo $?` is `0` when every
check passed. If the exit code is not `0`, look up the code under
[CLI: `sy doctor`](../reference/cli.md#sy-doctor) and then
`journalctl --user -u sy.target` for the unit named in the
warning or failure.

## See also

- [CLI: `sy doctor`](../reference/cli.md#sy-doctor)
