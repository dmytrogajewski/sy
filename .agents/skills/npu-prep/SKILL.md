---
name: npu-prep
description: Prepare an ONNX model for the AMD Ryzen AI NPU (export → BF16/INT8 → VitisAI compile cache)
allowed-tools: Bash(python *) Bash(source *) Bash(ls *) Bash(rm *) Bash(du *) Bash(grep *) Bash(find *) Bash(stat *) Bash(cat *) Read Edit Write
---

# NPU Model Prep

<constraints>
This skill operates the AMD venv at `/opt/AMD/ryzenai/venv` and writes
to `~/.cache/sy/aiplane/<stem>/`. Do not edit anything under
`/opt/AMD/` — it's vendor-managed.
</constraints>

<role>
NPU prep engineer. You take a Hugging Face transformer (or a raw ONNX)
and produce a VitisAI-compatible BF16 ONNX + a warm partition cache
that `sy-aiplane.service` can load instantly.
</role>

The prep pipeline mirrors `scripts/prep_npu_workload.py` (the
generalised successor to `scripts/prep_npu_embed.py`):

```
HF model id  ─→  static-shape ONNX  ─→  Quark BF16 QDQ  ─→  VitisAI compile cache
                  (export wrapper)      (--no-bf16-shrink if <2 GiB)
```

---

## Phase 1: Pick the workload + model

The workload determines:

- The **export wrapper** (what gets baked into the ONNX graph):
  - `embed`: mean-pool over last_hidden_state + L2-normalize.
  - `rerank`: cross-encoder; take `logits[..., 0]`, apply sigmoid.
  - `vad`: bare encoder; output speech probability per frame.
  - `stt`: encoder + decoder pair (Whisper-class).
  - `ocr`: detector (CRAFT-style) + recogniser (CRNN-style), exported
    separately and registered as two-stage in `aiplane::workloads::ocr`.
- The **input shape**: `(1, 512)` for text; `(1, 16000)` for 1 s VAD;
  `(1, 80, 3000)` for Whisper mel; `(1, 3, H, W)` for OCR.
- The **tokenizer**: shipped alongside ONNX when applicable.
- The **quant preset**: `BF16` (default) or `INT8_TRANSFORMER_DEFAULT`
  (small / latency-critical models like silero-vad).

State the choice up front:

```
workload: vad
model: snakers4/silero-vad
shape: (1, 1536)
tokenizer: none
preset: INT8_TRANSFORMER_DEFAULT
```

---

## Phase 2: Run the prep script

```
source /opt/AMD/ryzenai/venv/bin/activate
python scripts/prep_npu_workload.py \
    --workload <kind> \
    --quant-preset <BF16|INT8_TRANSFORMER_DEFAULT> \
    --no-bf16-shrink                 # if the BF16 model fits under 2 GiB
    --output-dir ~/.cache/sy/aiplane/<stem>/ \
    --json
```

Watch for these signals:

- **`[1/3] Exporting <model> → ONNX`** then **`[2/3] Quantising`** then
  **`[3/3] Compiling on NPU`**. The third step takes 3–10 min the
  first time and produces `compiled_<stem>_<cfg>/`.
- **`fail_safe_summary.json`** says `"AIE": 100, "CPU": 0` → 100% NPU
  offload (good). Anything less means the partition pass left ops on
  CPU and inference will be slower than expected — investigate which
  ops fell through and either replace them or accept the penalty.
- **`preliminary-vaiml-pass-summary.txt`** says
  `Number of operators supported by VAIML: X (Y%)`. Y ≥ 95% is the
  bar for an embedding/reranker.

If the compile bombs with `cannot find producer.
onnx_node_arg_name=…_DequantizeLinear_Output`: rerun with
`--no-bf16-shrink`. The shrink pass leaves dangling QDQ edges for
some models; small-enough models don't need it anyway.

---

## Phase 3: Smoke-test the compiled model

Inline Python smoke test (kept here, not in repo, since it's a
diagnostic, not a unit test):

```python
import numpy as np, onnxruntime as ort, time

sess = ort.InferenceSession(
    "/home/$USER/.cache/sy/aiplane/<stem>/<stem>.bf16.onnx",
    providers=["VitisAIExecutionProvider"],
    provider_options=[{
        "config_file": "/opt/AMD/ryzenai/venv/lib/python3.12/site-packages/voe-4.0-linux_x86_64/vaip_config.json",
        "cache_dir": "/home/$USER/.cache/sy/aiplane/<stem>/",
        "cache_key": "compiled_<stem>_<cfg>",
    }],
)
ids = np.zeros((1, 512), dtype=np.int64)
mask = np.ones((1, 512), dtype=np.int64)
t0 = time.time()
out = sess.run(None, {"input_ids": ids, "attention_mask": mask})
print(f"first {(time.time()-t0)*1000:.0f} ms  shape={out[0].shape}")
```

Expected: first inference under 1 s (cold), steady-state inferences
in the tens-to-low-hundreds of ms range for embed/rerank.

If it returns `unsupported data type 1` → check that `XILINX_XRT`
and `LD_LIBRARY_PATH` include `/opt/xilinx/xrt/lib`. The VAIML
custom op needs `libxrt_coreutil.so` reachable, otherwise the EP
silently downgrades to "generate-only" mode and the run path bombs.

---

## Phase 4: Register with sy-aiplane

If this is a **new** workload (not just refreshing an existing
model), see the `/workload` skill — it scaffolds the `Workload` impl,
the CLI surface, the MCP tool, and tests.

If this is **refreshing** an existing workload (model upgrade or
re-quant), simply restart the daemon:

```
sudo systemctl restart sy-aiplane.service
sy aiplane status --json | jq '.workloads["<kind>"]'
```

The daemon picks up the new artifact lazily on the next call to that
workload. To force-warm: `sy aiplane run --workload <kind> --in
'{"text":"warm"}'`.

---

## Phase 5: Document

Update the workload's `Workload` impl docstring with:

- Model id + revision pinned (or "tracking main")
- Input shape + tokenizer link
- VAIP partition % (from preliminary summary)
- Steady-state latency on Strix Point (measured)
- Quant preset used

This becomes the contract that any future re-prep must preserve.

---

<rules>
1. **Always use `--no-bf16-shrink` first** for BF16 models < 2 GiB.
   Only enable the shrink pass when the resulting model would otherwise
   exceed the 2 GiB protobuf cap.
2. **`compiled_<stem>_<cfg>/` is hot cache.** Don't delete it casually
   — recompile is 3–10 min. Stash it before re-prepping if you need
   to A/B test.
3. **Run smoke test before claiming success.** A clean prep + bad
   compile is a real failure mode (see `partition-info.json` and
   `fail_safe_summary.json`).
4. **The daemon owns the NPU.** Kill any `python` smoke-test process
   that still has `/dev/accel/accel0` open before restarting the
   daemon, or the daemon's session creation will EAGAIN.
</rules>
