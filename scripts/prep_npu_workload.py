#!/usr/bin/env python3
"""Prepare an ONNX model for the AMD Ryzen AI NPU via the VitisAI EP.

Generalised successor to `prep_npu_embed.py`. Dispatches per `--workload`
to the right export wrapper (mean-pool + L2-norm for embeddings,
sigmoid head for cross-encoder rerankers, raw encoder for STT, etc.),
quant preset, and tokenizer handling.

Run from the Ryzen AI venv:

    source /opt/xilinx/xrt/setup.sh
    source /opt/AMD/ryzenai/venv/bin/activate
    python ~/sources/sy/scripts/prep_npu_workload.py \\
        --workload embed \\
        --output-dir ~/.cache/sy/aiplane/multilingual-e5-base

For each kind, outputs land at
``~/.cache/sy/aiplane/<model-stem>/`` and the compiled VAIP cache
survives across daemon restarts.

Status: ``embed`` is fully implemented. ``rerank``, ``vad``, ``stt``,
and ``ocr`` are scaffolded — each prints the recipe and exits 2 so
the user (or the ``/workload`` skill) can fill in the export wrapper.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path
from typing import Optional


# =============================================================================
# Per-workload metadata
# =============================================================================

WORKLOAD_DEFAULTS = {
    "embed": {
        "model_id": "intfloat/multilingual-e5-base",
        "stem": "multilingual-e5-base",
        "seq_len": 512,
        "quant_preset": "BF16",
        "no_bf16_shrink": True,
        "ships_tokenizer": True,
    },
    "rerank": {
        "model_id": "BAAI/bge-reranker-v2-m3",
        "stem": "bge-reranker-v2-m3",
        "seq_len": 512,
        "quant_preset": "BF16",
        "no_bf16_shrink": True,
        "ships_tokenizer": True,
    },
    "vad": {
        "model_id": "snakers4/silero-vad",
        "stem": "silero-vad",
        "seq_len": 1536,
        "quant_preset": "INT8_TRANSFORMER_DEFAULT",
        "no_bf16_shrink": True,
        "ships_tokenizer": False,
    },
    "stt": {
        "model_id": "nvidia/parakeet-tdt-0.6b",
        "stem": "novasr",
        "seq_len": 3000,
        "quant_preset": "BF16",
        "no_bf16_shrink": False,
        "ships_tokenizer": True,
    },
    "ocr": {
        "model_id": "nvidia/nemotron-ocr-v2",
        "stem": "nemotron-ocr-v2",
        "seq_len": 0,  # image shape varies per stage
        "quant_preset": "BF16",
        "no_bf16_shrink": False,
        "ships_tokenizer": False,
    },
}


# =============================================================================
# Embed: full implementation
# =============================================================================

def export_onnx_embed(model_id: str, seq_len: int, out_path: Path) -> None:
    """Export an E5-style sentence encoder to ONNX with mean-pool +
    L2-normalize baked into the graph."""
    import torch
    from transformers import AutoModel, AutoTokenizer

    tok = AutoTokenizer.from_pretrained(model_id)
    base = AutoModel.from_pretrained(model_id)
    base.eval()

    tok.save_pretrained(out_path.parent / f"{out_path.stem}.tokenizer")

    class E5Wrapper(torch.nn.Module):
        """Mean-pool last_hidden_state with attention_mask, then L2-normalise."""

        def __init__(self, base):
            super().__init__()
            self.base = base

        def forward(self, input_ids, attention_mask):
            out = self.base(input_ids=input_ids, attention_mask=attention_mask)
            last = out.last_hidden_state                          # (B, T, H)
            mask = attention_mask.unsqueeze(-1).to(last.dtype)    # (B, T, 1)
            summed = (last * mask).sum(dim=1)                     # (B, H)
            counts = mask.sum(dim=1).clamp(min=1e-9)              # (B, 1)
            pooled = summed / counts                              # (B, H)
            return torch.nn.functional.normalize(pooled, p=2, dim=1)

    wrapper = E5Wrapper(base)
    wrapper.eval()

    dummy_text = "exporting to onnx " * 64
    enc = tok(dummy_text, return_tensors="pt", padding="max_length",
              truncation=True, max_length=seq_len)
    input_ids = enc["input_ids"].to(torch.int64)
    attn = enc["attention_mask"].to(torch.int64)

    out_path.parent.mkdir(parents=True, exist_ok=True)
    torch.onnx.export(
        wrapper,
        (input_ids, attn),
        out_path.as_posix(),
        input_names=["input_ids", "attention_mask"],
        output_names=["sentence_embedding"],
        dynamic_axes=None,
        opset_version=17,
    )
    # Re-save with external data so downstream tools (quark, onnxsim)
    # handle the >2 GB protobuf cap cleanly.
    import onnx
    model = onnx.load(out_path.as_posix(), load_external_data=True)
    onnx.save_model(
        model,
        out_path.as_posix(),
        save_as_external_data=True,
        all_tensors_to_one_file=True,
        location=out_path.name + ".data",
        size_threshold=1024,
    )


# =============================================================================
# Rerank / VAD / STT / OCR: scaffolded
# =============================================================================

def export_onnx_rerank(model_id: str, seq_len: int, out_path: Path) -> None:
    raise NotImplementedError(
        f"rerank export not yet implemented for {model_id}.\n"
        f"Follow the /workload skill (.claude/commands/workload.md):\n"
        f"  1. Load AutoModelForSequenceClassification.\n"
        f"  2. Wrap with a module that returns logits[..., 0].sigmoid().\n"
        f"  3. Export to ({out_path}) with input_ids + attention_mask\n"
        f"     shape (1, {seq_len}).\n"
        f"  4. Update aiplane::workloads::rerank::run() to encode\n"
        f"     (query, doc) as one concatenated XLM-RoBERTa sequence.\n"
    )


def export_onnx_vad(model_id: str, seq_len: int, out_path: Path) -> None:
    raise NotImplementedError(
        f"vad export not yet implemented for {model_id}.\n"
        f"silero-vad ships pre-built ONNX — fetch and copy:\n"
        f"  wget https://github.com/snakers4/silero-vad/raw/master/"
        f"src/silero_vad/data/silero_vad.onnx -O {out_path}\n"
        f"Then update aiplane::workloads::vad::run() to feed 32 ms\n"
        f"frames and post-process the frame probabilities into\n"
        f"speech spans with hysteresis."
    )


def export_onnx_stt(model_id: str, seq_len: int, out_path: Path) -> None:
    raise NotImplementedError(
        f"stt export not yet implemented for {model_id}.\n"
        f"Whisper/Parakeet are two-stage (encoder + decoder); split the\n"
        f"export into <stem>.encoder.bf16.onnx + <stem>.decoder.onnx.\n"
        f"Reference: RyzenAI-SW/Demos/ASR/ exports.\n"
        f"For the locally-available novasr, see\n"
        f"  ~/sources/novasr-output/data/model/novasr.onnx\n"
        f"and the export script in that repo."
    )


def export_onnx_ocr(model_id: str, seq_len: int, out_path: Path) -> None:
    raise NotImplementedError(
        f"ocr export not yet implemented for {model_id}.\n"
        f"Nemotron OCR v2 is two-stage (detector + recogniser);\n"
        f"RyzenAI-SW ships the export scripts:\n"
        f"  ~/sources/RyzenAI-SW/Examples/nemotron-ocr-v2/\n"
        f"Produce {out_path.parent}/detector.bf16.onnx and\n"
        f"{out_path.parent}/recogniser.bf16.onnx; aiplane::workloads::ocr\n"
        f"chains them under a single NPU mutex acquisition."
    )


EXPORTERS = {
    "embed": export_onnx_embed,
    "rerank": export_onnx_rerank,
    "vad": export_onnx_vad,
    "stt": export_onnx_stt,
    "ocr": export_onnx_ocr,
}


# =============================================================================
# Shared: quantise (Quark) + warm (VAIP)
# =============================================================================

def quantize(in_path: Path, out_path: Path, preset: str,
             bf16_shrink: bool = True) -> None:
    """Quark BF16/INT8 quantise. The monkey-patches work around two
    Quark 0.11rc1 bugs in `create_infer_session_for_onnx_model` that
    bomb on >2 GB models because they forget to forward
    `use_external_data_format`."""
    from quark.onnx import ModelQuantizer
    from quark.onnx.quantization.config import Config, get_default_config

    import quark.onnx.utils.model_utils as _mu
    import quark.onnx.quantization.quantize as _qz
    import quark.onnx.calibration.data_readers as _dr
    _orig_make = _mu.create_infer_session_for_onnx_model

    def _make_session(model_input, sess_options=None,
                      use_external_data_format=True, *args, **kwargs):
        return _orig_make(model_input, sess_options=sess_options,
                          use_external_data_format=use_external_data_format,
                          *args, **kwargs)

    _mu.create_infer_session_for_onnx_model = _make_session
    _dr.create_infer_session_for_onnx_model = _make_session
    if hasattr(_qz, "create_infer_session_for_onnx_model"):
        _qz.create_infer_session_for_onnx_model = _make_session

    cfg = get_default_config(preset)
    if preset == "BF16":
        cfg.extra_options["BF16QDQToCast"] = True
    cfg.extra_options["UseRandomData"] = True
    cfg.use_external_data_format = True

    try:
        quantizer = ModelQuantizer(Config(global_quant_config=cfg))
        quantizer.quantize_model(
            model_input=in_path.as_posix(),
            model_output=out_path.as_posix(),
            calibration_data_path=None,
        )
    finally:
        _mu.create_infer_session_for_onnx_model = _orig_make
        _dr.create_infer_session_for_onnx_model = _orig_make
        if hasattr(_qz, "create_infer_session_for_onnx_model"):
            _qz.create_infer_session_for_onnx_model = _orig_make

    if preset == "BF16" and bf16_shrink:
        _shrink_fp32_initializers_to_bf16(out_path)


def _shrink_fp32_initializers_to_bf16(path: Path) -> None:
    """Rewrite every FP32 initializer feeding only a Cast(FP32→BF16)
    to store data as BFLOAT16 in raw_data. Halves the on-disk weight
    bytes and avoids tripping protobuf's 2 GiB cap during VAIP
    partition serialisation. Set ``--no-bf16-shrink`` for models that
    fit under the cap (the shrink can leave dangling QDQ edges that
    the partition pass rejects)."""
    import numpy as np
    import onnx
    from onnx import TensorProto, numpy_helper

    model = onnx.load(path.as_posix(), load_external_data=True)
    g = model.graph
    consumers: dict[str, list[tuple[int, int, "onnx.NodeProto"]]] = {}
    for ni, node in enumerate(g.node):
        for ii, name in enumerate(node.input):
            consumers.setdefault(name, []).append((ni, ii, node))

    converted = 0
    for init in list(g.initializer):
        if init.data_type != TensorProto.FLOAT:
            continue
        consumes = consumers.get(init.name, [])
        if not consumes:
            continue
        only_cast_to_bf16 = True
        for _, ii, node in consumes:
            if node.op_type != "Cast" or ii != 0:
                only_cast_to_bf16 = False
                break
            to_attr = next((a for a in node.attribute if a.name == "to"), None)
            if to_attr is None or to_attr.i != TensorProto.BFLOAT16:
                only_cast_to_bf16 = False
                break
        if not only_cast_to_bf16:
            continue

        arr_fp32 = numpy_helper.to_array(init).astype(np.float32, copy=False)
        u32 = arr_fp32.view(np.uint32)
        bias = 0x7FFF + ((u32 >> 16) & 1)
        u16 = ((u32 + bias) >> 16).astype(np.uint16)
        new = onnx.TensorProto()
        new.name = init.name
        new.data_type = TensorProto.BFLOAT16
        new.dims.extend(init.dims)
        new.raw_data = u16.tobytes()
        g.initializer.remove(init)
        g.initializer.append(new)
        for ni, _, node in consumes:
            cast_out = node.output[0]
            for other in g.node:
                for k, name in enumerate(other.input):
                    if name == cast_out:
                        other.input[k] = init.name
            node.op_type = "Identity"
            attrs_keep = [a for a in node.attribute if a.name != "to"]
            del node.attribute[:]
            node.attribute.extend(attrs_keep)
        converted += 1

    print(f"  bf16-shrink: converted {converted} initializers",
          file=sys.stderr)
    data_name = path.name + ".data"
    if (path.parent / data_name).exists():
        (path.parent / data_name).unlink()
    onnx.save_model(
        model,
        path.as_posix(),
        save_as_external_data=True,
        all_tensors_to_one_file=True,
        location=data_name,
        size_threshold=1024,
    )


def warm_npu_cache(bf16_path: Path, vaip_config: Path, cache_dir: Path,
                   cache_key: str) -> dict:
    """Compile + dry-run inference. Produces
    ``<cache_dir>/<cache_key>/vaiml_par_0/`` with the partition
    artifacts; daemon picks it up on next session creation."""
    import numpy as np
    import onnxruntime as ort

    cache_dir.mkdir(parents=True, exist_ok=True)
    so = ort.SessionOptions()
    so.log_severity_level = 2
    t0 = time.time()
    sess = ort.InferenceSession(
        bf16_path.as_posix(),
        sess_options=so,
        providers=["VitisAIExecutionProvider"],
        provider_options=[{
            "config_file": vaip_config.as_posix(),
            "cache_dir": cache_dir.as_posix(),
            "cache_key": cache_key,
        }],
    )
    compile_seconds = round(time.time() - t0, 2)

    inputs = sess.get_inputs()
    feed = {}
    for inp in inputs:
        shape = [d if isinstance(d, int) and d > 0 else 1 for d in inp.shape]
        dtype = np.int64 if "int" in inp.type else np.float32
        feed[inp.name] = np.zeros(shape, dtype=dtype)
        if inp.name == "attention_mask":
            feed[inp.name] = np.ones(shape, dtype=np.int64)
    t0 = time.time()
    out = sess.run(None, feed)
    inference_ms = round((time.time() - t0) * 1000, 2)
    return {
        "compile_seconds": compile_seconds,
        "first_inference_ms": inference_ms,
        "output_shapes": [list(o.shape) for o in out],
        "output_dtypes": [str(o.dtype) for o in out],
    }


# =============================================================================
# Main
# =============================================================================

def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--workload", required=True,
                    choices=list(WORKLOAD_DEFAULTS.keys()),
                    help="Which workload to prep.")
    ap.add_argument("--model-id", default=None,
                    help="Override the workload's default HF model id.")
    ap.add_argument("--seq-len", type=int, default=None,
                    help="Override input sequence length / shape dimension.")
    ap.add_argument("--quant-preset", default=None,
                    help="Quark preset (BF16, INT8_TRANSFORMER_DEFAULT, ...).")
    ap.add_argument("--output-dir", type=Path, default=None,
                    help="Output dir. Defaults to ~/.cache/sy/aiplane/<stem>/.")
    ap.add_argument("--vaip-config", type=Path,
                    default=Path("/opt/AMD/ryzenai/venv/lib/python3.12/"
                                 "site-packages/voe-4.0-linux_x86_64/"
                                 "vaip_config.json"))
    ap.add_argument("--skip-warm", action="store_true",
                    help="Skip the NPU compile/warmup step.")
    ap.add_argument("--no-bf16-shrink", action="store_true",
                    help="Force-skip the BF16 initializer shrink pass. "
                         "Some workloads default to this anyway "
                         "(see WORKLOAD_DEFAULTS).")
    ap.add_argument("--bf16-shrink", action="store_true",
                    help="Force-enable the shrink even if the workload "
                         "default would skip it.")
    ap.add_argument("--json", action="store_true",
                    help="Emit a final JSON summary on stdout.")
    args = ap.parse_args()

    defaults = WORKLOAD_DEFAULTS[args.workload]
    model_id = args.model_id or defaults["model_id"]
    seq_len = args.seq_len if args.seq_len is not None else defaults["seq_len"]
    preset = args.quant_preset or defaults["quant_preset"]
    if args.bf16_shrink:
        do_shrink = True
    elif args.no_bf16_shrink:
        do_shrink = False
    else:
        do_shrink = not defaults["no_bf16_shrink"]

    if not args.vaip_config.is_file():
        print(f"error: vaip config not found: {args.vaip_config}",
              file=sys.stderr)
        print("hint: did you `source /opt/AMD/ryzenai/venv/bin/activate`?",
              file=sys.stderr)
        return 2

    stem = defaults["stem"]
    out_dir = (args.output_dir or
               (Path.home() / ".cache/sy/aiplane" / stem)).expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)
    fp32 = out_dir / f"{stem}.onnx"
    suffix = preset.lower()
    quant = out_dir / f"{stem}.{suffix}.onnx"
    cache_key = f"compiled_{stem}_{suffix}_seq{seq_len}"

    summary: dict = {
        "workload": args.workload,
        "model_id": model_id,
        "model_stem": stem,
        "seq_len": seq_len,
        "quant_preset": preset,
        "output_dir": str(out_dir),
    }

    print(f"[1/3] Exporting {model_id} → ONNX (workload={args.workload}, seq_len={seq_len})",
          file=sys.stderr)
    if not fp32.is_file():
        try:
            EXPORTERS[args.workload](model_id, seq_len, fp32)
        except NotImplementedError as e:
            print(f"\n{e}", file=sys.stderr)
            return 2
    else:
        print(f"      reusing existing {fp32.name}", file=sys.stderr)
    summary["fp32_path"] = str(fp32)
    summary["fp32_bytes"] = fp32.stat().st_size

    print(f"[2/3] Quantising → {preset} (bf16_shrink={do_shrink})",
          file=sys.stderr)
    if not quant.is_file():
        quantize(fp32, quant, preset, bf16_shrink=do_shrink)
    else:
        print(f"      reusing existing {quant.name}", file=sys.stderr)
    summary["quant_path"] = str(quant)
    summary["quant_bytes"] = quant.stat().st_size

    if args.skip_warm:
        print(f"[3/3] Skipping NPU warm (--skip-warm)", file=sys.stderr)
    else:
        print(f"[3/3] Compiling on NPU (~3 min on first run)", file=sys.stderr)
        summary.update(warm_npu_cache(quant, args.vaip_config, out_dir, cache_key))
        summary["cache_dir"] = str(out_dir / cache_key)

    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        for k, v in summary.items():
            print(f"  {k:24s} {v}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
