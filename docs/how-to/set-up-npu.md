<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to set up the NPU

## Goal

Install the AMD Ryzen AI runtime, compile the embed workload for the
XDNA NPU, and confirm `sy aiplane` serves embeddings from
`/dev/accel/accel0`.

## Prerequisites

- Fedora 43 on an AMD Ryzen AI laptop (Phoenix / Strix, with
  `/dev/accel/accel0` once the kernel module is loaded). If you do
  not have that hardware, stop here: the knowledge plane already
  falls back to CPU and you do not need this how-to.
- You completed
  [the bring-up tutorial](../tutorials/getting-started.md). `sy` is
  on `$PATH`.
- `sudo` on the host.
- Disk space for the compile cache (a few gigabytes under
  `~/.cache/sy/npu-embed/`).

## Steps

1. Install the AMD Ryzen AI 1.7.1 system packages from the companion
   repo [`ryzenai-rpm`](https://github.com/dmytrogajewski/ryzenai-rpm)
   (XRT runtime, XDNA DKMS module, memlock config, AMD's Python
   wheel set). Follow that repo's install instructions. When it is
   done, these two paths exist:

   ```bash
   ls /dev/accel/accel0
   ls /opt/AMD/ryzenai/venv/bin/activate
   ```

2. Load the AMD venv and compile the embed workload from this repo.
   The script downloads `intfloat/multilingual-e5-base` from Hugging
   Face, exports ONNX, BF16-quantises with Quark, and runs a one-shot
   VitisAI compile. Output lands in `~/.cache/sy/npu-embed/`. The
   compile is slow the first time and then reused:

   ```bash
   source /opt/AMD/ryzenai/venv/bin/activate
   python ~/sources/sy/scripts/prep_npu_workload.py
   ```

3. Restart the planes so `aiplane` picks up AMD's libraries. On
   start it re-execs itself with `LD_LIBRARY_PATH` pointing at the
   Ryzen AI runtime (see [glossary: re-exec dance](../reference/glossary.md#re-exec-dance)):

   ```bash
   systemctl --user restart sy.target
   ```

4. Confirm the NPU plane is up and the embed backend is `vitisai`:

   ```bash
   sy aiplane status --json
   sy knowledge status --json
   ```

   Look at `embed_backend` on the knowledge status document. It
   should read `vitisai`. If it reads `cpu`, the venv was not
   detected; check that `/opt/AMD/ryzenai/venv` exists and that you
   restarted `sy.target` after installing it.

## Result

`/dev/accel/accel0` is owned by the `aiplane` daemon, embeddings
run on the NPU, and the GPU stays free for other work. One-shot
CLI calls go through the daemon's socket; do not start a second
ORT session against the device.

## See also

- [Why embeddings run on the NPU, not the GPU](../explanation/why-npu-not-gpu.md)
- [Glossary: re-exec dance](../reference/glossary.md#re-exec-dance)
- [Glossary: VitisAI EP](../reference/glossary.md#vitisai-ep)
