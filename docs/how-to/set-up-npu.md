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
  `~/.cache/sy/aiplane/<model-stem>/`). Treat that tree as
  *rebuildable*, not *backed up*: `prep_npu_workload.py` regenerates
  it, and `sy doctor` reports `aiplane.model_artifacts` as a failure
  the moment a cache sweep prunes it (see Step 6).

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

2. Load the venv and compile the two workloads the knowledge daemon
   raises at start-up. Each run downloads the Hugging Face model,
   exports ONNX, quantises with Quark, and does a one-shot VitisAI
   compile (≈1.5 min for `embed`):

   ```bash
   source /opt/xilinx/xrt/setup.sh
   source /opt/AMD/ryzenai/venv/bin/activate
   python ~/sources/sy/scripts/prep_npu_workload.py --workload embed
   ```

   Artifacts land in `~/.cache/sy/aiplane/<model-stem>/`: the FP32
   export (`<stem>.onnx` + `.data`), the BF16 NPU graph
   (`<stem>.bf16.onnx` + `.data`), the tokenizer directory, and the
   compiled partition `compiled_<stem>_bf16_seq<N>_<tail>/`.

   The partition directory name **is** the VitisAI cache key and must
   equal the key the Rust worker requests — `prep_npu_workload.py`
   derives it from the workload table for exactly that reason. Pass
   `--cache-key` only if you also change the Rust side: a partition
   compiled under the wrong key is simply never looked up: `embed` and
   `rerank` then pay the full AIE codegen cost on the first daemon start,
   while `stt` refuses to load and tells you to re-run prep.

3. Speech (`stt`) is optional and prep-managed separately. The Whisper
   artefacts arrive VAIML-*partitioned* (the graph is cut for the AIE) but
   not *compiled*: the `.rai` bitstream for each of the two partitions is
   built locally, ~6 min and ~19.5 GiB peak RSS in total. That belongs here,
   never at runtime — `sy-knowledge.service` runs under `MemoryHigh`, and an
   over-cap compile there gets parked in direct reclaim and never finishes
   while the host idles with free RAM. The worker therefore refuses a cold
   compile and points back at this command:

   ```bash
   python ~/sources/sy/scripts/prep_npu_workload.py --workload stt
   ```

   It snapshots `amd/whisper-medium-onnx-npu` plus the tokenizer into
   `~/.cache/sy/aiplane/whisper-medium/`, vendors the two VitisAI configs,
   and compiles `whisper_medium_encoder/` and `whisper_medium_decoder/`.
   Re-running is cheap: existing artefacts are reused (no re-download) and a
   warm partition loads in seconds. `--skip-warm` fetches without compiling,
   which leaves `sy aiplane run --workload stt` refusing to load until the
   compile is done. Verify with the AMD LibriSpeech sample:

   ```bash
   # PCM exceeds argv, so hand it to the CLI through a file.
   python3 -c 'import json,struct,sys,wave;w=wave.open(sys.argv[1]);r=w.readframes(w.getnframes());json.dump({"kind":"audio","sr":w.getframerate(),"pcm":list(struct.unpack("<%dh"%(len(r)//2),r))},open(sys.argv[2],"w"))' \
     ~/sources/RyzenAI-SW/Demos/ASR/Whisper/audio_files/1089-134686-0000.wav /tmp/stt_input.json
   sy aiplane run --workload stt --json --in-file /tmp/stt_input.json
   ```

   Expect "He hoped there would be stew for dinner, turnips and carrots
   …". A cold daemon-side load takes ~7 s once the partitions exist.

4. Restart the planes so `aiplane` picks up AMD's libraries. On
   start it re-execs itself with `LD_LIBRARY_PATH` pointing at the
   Ryzen AI runtime (see [glossary: re-exec dance](../reference/glossary.md#re-exec-dance)):

   ```bash
   systemctl --user restart sy.target
   ```

5. Confirm the NPU plane is up and the embed backend is `vitisai`:

   ```bash
   sy aiplane status --json
   sy knowledge status --json
   ```

   Look at `embed_backend` on the knowledge status document. It
   should read `vitisai`. If it reads `cpu`, the venv was not
   detected; check that `/opt/AMD/ryzenai/venv` exists and that you
   restarted `sy.target` after installing it.

6. If the tile in the bar ever shows `🧠 !`, or `sy knowledge status`
   says `daemon: down`, the artifacts are the first thing to check —
   `~/.cache` is a legitimate cleanup target and the plane notices:

   ```bash
   sy doctor --json | jq '.checks[] | select(.name=="aiplane.model_artifacts")'
   ```

   A `Fail` there names the missing path (calling out a dangling
   symlink) and prints the exact rebuild command. Re-run Step 2, then:

   ```bash
   systemctl --user restart sy-knowledge.service
   ```

   A missing **`rerank`** artifact is a `Warn`, not a `Fail`: the
   reranker only re-scores hits, so search and indexing keep running.
   A missing **`embed`** artifact stops indexing, and the daemon comes
   up degraded instead of exiting — the bar tile, `sy doctor`, and
   `status.last_error` all carry the reason, and the plane heals
   itself once the artifact returns.

## Result

`/dev/accel/accel0` is owned by the `aiplane` daemon, embeddings
run on the NPU, and the GPU stays free for other work. One-shot
CLI calls go through the daemon's socket; do not start a second
ORT session against the device.

## See also

- [Why embeddings run on the NPU, not the GPU](../explanation/why-npu-not-gpu.md)
- [Glossary: re-exec dance](../reference/glossary.md#re-exec-dance)
- [Glossary: VitisAI EP](../reference/glossary.md#vitisai-ep)
