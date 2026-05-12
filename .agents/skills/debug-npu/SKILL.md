---
name: debug-npu
description: Runbook for diagnosing AMD Ryzen AI NPU failures on sy
allowed-tools: Bash(cat *) Bash(ls *) Bash(ps *) Bash(pgrep *) Bash(lsof *) Bash(sudo journalctl *) Bash(sudo systemctl *) Bash(nvidia-smi *) Bash(strings *) Bash(grep *) Bash(rg *) Bash(find *) Bash(stat *) Read
---

# NPU Debug Runbook

<constraints>
This is a *diagnostic* skill. It reads system state; it does NOT
modify the system unless the user explicitly approves a fix step
(systemctl restart, file deletion, etc.). All inferences are
captured for the bug report (`/bug`) or roadmap item (`/roadmap`)
that follows.
</constraints>

<role>
NPU-plane diagnostician. You know that the same symptom — "the daemon
runs but returns errors" — can come from a dozen unrelated layers
(memlock, fd cap, SELinux label, model cache corruption, qdrant
dimension mismatch, model not loaded yet, …). You triage methodically
top-down, capturing evidence as you go.
</role>

---

## Top-down decision tree

Run the probes in order. Stop at the first probe that reveals the
fault.

### 0. Is the daemon up at all?

```
sy aiplane status --json | jq '.daemon_running, .qdrant_ready'
sudo systemctl status sy-aiplane.service --no-pager | head -15
pgrep -af 'sy aiplane daemon' | grep -v claude-shell
```

- `daemon_running: false` → unit dead. Look at the unit's recent
  journal (`sudo journalctl -u sy-aiplane -n 50 --no-pager`).
- `qdrant_ready: false` → child process down or unreachable. Check
  qdrant fd count and port. See "Gate 4" below.

### 1. Is the NPU even there?

```
cat /sys/class/accel/accel0/device/power_state
cat /sys/class/accel/accel0/device/fw_version
cat /sys/class/accel/accel0/device/power/runtime_status
rpm -qa | grep -E 'amdxdna|xrt|ryzenai'
```

- `/sys/class/accel/accel0` missing → kernel module not loaded, or
  the BIOS NPU toggle is off, or no XDNA hardware.
- `runtime_status: suspended` while you expect activity → workload
  hasn't been invoked yet or just finished; check the daemon's
  embed_backend in status to confirm it loaded.

### 2. Did the AMD env re-exec fire?

```
PID=$(pgrep -f 'sy aiplane daemon' | head -1)
tr '\0' '\n' < /proc/$PID/environ | grep -E 'SY_AMD_REEXECED|LD_LIBRARY_PATH|ORT_DYLIB_PATH|XILINX_'
```

- `SY_AMD_REEXECED=1` must be present. Missing → the re-exec was
  skipped (probably `/opt/AMD/ryzenai/venv` missing or
  `libonnxruntime.so.*` missing inside it).
- `LD_LIBRARY_PATH` must include
  `/opt/AMD/ryzenai/venv/lib/python3.12/site-packages/onnxruntime/capi`,
  voe/lib, flexml/flexml_extras/lib, vaimlpl_be/lib, flexmlrt/lib,
  and `/opt/xilinx/xrt/lib`. Missing any → VitisAI EP load fails.

### 3. Are capabilities in place?

```
grep -E '^Cap|^Uid|^Gid' /proc/$PID/status
cat /proc/$PID/limits | grep -E 'Max locked|Max open files'
```

- `CapAmb` must include `CAP_IPC_LOCK` (bit 0x4000). Missing →
  the systemd unit isn't granting it, or you're running the daemon
  outside systemd. Fix: `sy aiplane install-service` and
  `sudo systemctl restart sy-aiplane.service`.
- `Max locked memory` must be `unlimited` (or large enough for the
  64 MiB DRM heap mmap). 8 MiB → `LimitMEMLOCK` missing from unit.
- `Max open files` must be ≥ 524288. 1024 → `LimitNOFILE` missing
  from unit; qdrant will choke at ~64 segments.

### 4. Is qdrant actually responsive?

```
curl -sS -m 3 http://127.0.0.1:6333/readyz
ss -ltnp | grep -E ':6333|:6334'
QDPID=$(pgrep -f /qdrant | head -1)
ls /proc/$QDPID/fd | wc -l
```

- `curl` timing out while qdrant is "listening" → fd exhaustion.
  Count of fds approaching the soft cap is the smoking gun. Restart
  the unit (`sudo systemctl restart sy-aiplane.service`); confirm
  `LimitNOFILE` is correct.
- qdrant process gone → check journal for OOM kill or panic.

### 5. NPU session creation failing?

Read the journal for the FlexMLRT exception:

```
sudo journalctl -u sy-aiplane --no-pager -n 100 | rg -E 'FlexMLRT|VitisAI|mmap|EAGAIN|err=-?\d+'
```

- `mmap(...) failed (err=-11): Resource temporarily unavailable` →
  no `CAP_IPC_LOCK` or `LimitMEMLOCK`. Gate 3 should have caught it.
- `XRT is not installed` →`/opt/xilinx/xrt/lib` not in
  `LD_LIBRARY_PATH`. Gate 2 should have caught it.
- `cannot find producer. onnx_node_arg_name=…_DequantizeLinear_Output`
  → the model's BF16 shrink left dangling QDQ edges. Re-prep with
  `--no-bf16-shrink`. See `/npu-prep`.
- `unsupported data type 1` → EP loaded but XRT runtime is in
  "generate-only" mode. Same fix as `XRT is not installed`.
- `cannot find producer` on a fresh model → compile cache corruption.
  Delete `~/.cache/sy/aiplane/<stem>/compiled_*` and rerun the prep
  script.

### 6. Model cache present + intact?

```
ls -la ~/.cache/sy/aiplane/<stem>/
ls -la ~/.cache/sy/aiplane/<stem>/compiled_<stem>_*/vaiml_par_0/
cat ~/.cache/sy/aiplane/<stem>/compiled_<stem>_*/vaiml_par_0/fail_safe_summary.json
```

- Missing `<stem>.bf16.onnx` → run prep via `/npu-prep`.
- `fail_safe_summary.json` shows `"CPU": >0` → partition pass didn't
  put 100% on AIE. Some ops fell to CPU. Acceptable for some
  workloads (denoise, OCR with non-AIE-supported ops); investigate
  if performance is wrong.
- `original-info-signature.txt` / `original-model-signature.txt` in
  `$PWD` of the smoke-test process → VAIP dropped them while
  running; harmless but should be in `.gitignore`.

### 7. CLI consumer fell through to CUDA?

```
sy knowledge search "test" 2>&1 | rg 'CUDA|VitisAI|CPU'
```

If you see `embeddings on NVIDIA … via CUDA`, the CLI tried VitisAI
(got EAGAIN because the daemon owns the NPU), tried CUDA (succeeded
because libonnxruntime has the EP built in), and now you're burning
GPU VRAM for a one-shot query. **This is wrong** after commit
`d2b1b1e` — the CLI should delegate to the daemon over IPC. If you
see this:

- Verify the new code is installed: `ls -la ~/.local/bin/sy` mtime,
  and check `strings ~/.local/bin/sy | rg 'request.*Req::Search'`.
- Verify the daemon is fresh too: `sudo systemctl status
  sy-aiplane.service` and check the daemon's reported version.
- If both are fresh: check `aiplane::cli::try_daemon_search`
  liveness probe — `sy aiplane status` must return
  `daemon_running: true` with `is_fresh()` true. If it doesn't, the
  CLI legitimately falls back.

---

## Common fixes (with confirmation prompts)

After identifying the root cause, ask the user before applying any
of these:

- `sy aiplane install-service` — rewrites the systemd unit (sudo
  prompts).
- `sudo systemctl restart sy-aiplane.service` — restarts the daemon.
- `rm -rf ~/.cache/sy/aiplane/<stem>/compiled_<stem>_*/` — flushes
  compile cache; recompile is 3–10 min.
- `python scripts/prep_npu_workload.py --workload <kind> …` —
  re-prep the model.

Never apply these silently.

---

## Hand-off

When done diagnosing, hand off to:

- `/bug` skill if this is a real defect with a test that can capture
  it.
- The user directly if it's an environment / configuration issue
  (missing AMD packages, SELinux denial that needs `audit2allow`,
  hardware issue).

---

<rules>
1. **Probe top-down.** Don't grep for `mmap` first — that's the
   symptom, not the cause.
2. **Capture evidence.** Each probe's output goes into the bug doc
   if a `/bug` follow-up lands.
3. **One layer at a time.** Don't change three things and hope.
4. **Ask before doing.** Restarts, cache deletions, and re-preps are
   user-approved actions.
5. **NPU is single-context.** If both the daemon and a CLI process
   are trying to attach, you've already found the bug.
</rules>
