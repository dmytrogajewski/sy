---
name: Bug report
about: Report a defect in sy — a plane misbehaves, a command exits non-zero, or `sy apply` leaves the host in an unexpected state
title: 'bug: '
labels: bug
assignees: ''
---

<!-- Rendered from the `/documenter issue-templates` skill, anchored on
     the project's `/bug` workflow (`.agents/skills/bug/SKILL.md`):
     reproduce first, then file. If you cannot reproduce the bug, open
     a discussion instead. -->

> **Security-sensitive bug?** Stop and read
> [`SECURITY.md`](../../SECURITY.md). Do **not** file a public issue
> for anything affecting the `syauth` PAM module, `CAP_*` ambient
> grants on the user daemon, SELinux policy, the polkit rule, or
> ownership of `/dev/accel/accel0`.

### What did you expect to happen?

<!-- One or two sentences. Imperative voice — "`sy aiplane status`
     should print one JSON object per registered workload." -->

### What actually happened?

<!-- The exact stderr, the exit code, and the visible journal entries.
     Paste verbatim inside a fenced block. -->

```
<paste stderr / output / journalctl excerpt here>
```

### Reproduction steps

<!-- Numbered, copy-pasteable. A single `sy ...` invocation or a
     minimal `configs/` diff is ideal. If the bug needs a clean state,
     include the cleanup command. -->

1.
2.
3.

### Affected plane

<!-- Tick all that apply. -->

- [ ] `aiplane` (NPU inference, `/dev/accel/accel0`, workloads)
- [ ] `agt` (sandboxed agent runner)
- [ ] `knowledge` (qdrant + semantic search)
- [ ] `power` (governor, `sy-powerd`)
- [ ] `stack` (layer-shell bar, waybar integration)
- [ ] `syauth` (phone-as-key sudo, PAM)
- [ ] `supervision` (`sy.target`, systemd units under `configs/systemd/`)
- [ ] `sy apply` (declarative layer, `configs/` rendering)
- [ ] CLI / IPC envelope (cross-plane)
- [ ] Docs / specs

### Environment

<!-- Run each command and paste the one-line output. Mark "n/a" if
     a command does not apply. -->

- `sy --version`:
- `cat /etc/fedora-release`:
- `rustc --version`:
- `cargo --version`:
- `uname -r` (kernel):
- Display server: <!-- niri / sway / other -->

### NPU context (only if the bug touches `aiplane`)

<!-- Skip this block if the bug is not NPU-related. -->

- `ls /dev/accel/accel0` (present / absent):
- `cat /sys/class/accel/accel0/device/power_state` (D0 / D3 / other):
- Daemon running? `pgrep -af 'sy aiplane'`:
- AMD venv present? `ls /opt/AMD/ryzenai/venv/lib | head -1`:
- Re-exec confirmed? Check
  `grep -z SY_AMD_REEXECED /proc/$(pgrep -f 'sy aiplane daemon')/environ`:
- `sy aiplane status --json` (paste output, redact paths if needed):

```json
<paste sy aiplane status --json output>
```

- `sy doctor --json` (paste output):

```json
<paste sy doctor --json output>
```

### Additional context

<!-- Anything else relevant: recent `git log -5 --oneline`, a link to a
     related `specs/bugs/BUG-*.md` file, a screenshot of the layer-shell
     bar if visual, etc. -->
