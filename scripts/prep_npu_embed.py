#!/usr/bin/env python3
"""Export intfloat/e5-model to a BF16 ONNX model that AMD's
VitisAI EP can partition onto the Ryzen AI NPU.

E5 sentence embeddings are mean-pooled (mask-weighted) over the encoder
output then L2-normalised. This script bakes that into the ONNX graph
so the runtime gets a single (1, 1024) embedding vector per call.

Run from the Ryzen AI venv:
    source /opt/xilinx/xrt/setup.sh
    source /opt/AMD/ryzenai/venv/bin/activate
    python ~/sources/sy/scripts/prep_npu_embed.py \
        --output-dir ~/.cache/sy/npu-embed \
        --seq-len 512

Outputs:
    <output-dir>/e5-model.onnx         (FP32, static shape)
    <output-dir>/e5-model.bf16.onnx    (BF16 QDQ, NPU-ready)
    <output-dir>/compiled_e5-model/    (VitisAI cache, after first run)

The compiled cache survives across sy daemon restarts; you only pay the
~3 min aiecompiler cost once per (model, seq_len) tuple.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path


def export_onnx(model_id: str, seq_len: int, out_path: Path) -> None:
    import torch
    from transformers import AutoModel, AutoTokenizer

    tok = AutoTokenizer.from_pretrained(model_id)
    base = AutoModel.from_pretrained(model_id)
    base.eval()

    # Ship the tokenizer alongside the ONNX so the Rust side can load it
    # via the `tokenizers` crate without re-downloading from HF.
    tok.save_pretrained(out_path.parent / f"{out_path.stem}.tokenizer")

    class E5Wrapper(torch.nn.Module):
        """Mean-pool last_hidden_state with attention_mask, then L2-normalise."""

        def __init__(self, base):
            super().__init__()
            self.base = base

        def forward(self, input_ids, attention_mask):
            out = self.base(input_ids=input_ids, attention_mask=attention_mask)
            last = out.last_hidden_state  # (B, T, H)
            mask = attention_mask.unsqueeze(-1).to(last.dtype)  # (B, T, 1)
            summed = (last * mask).sum(dim=1)  # (B, H)
            counts = mask.sum(dim=1).clamp(min=1e-9)  # (B, 1)
            pooled = summed / counts  # (B, H)
            return torch.nn.functional.normalize(pooled, p=2, dim=1)

    wrapper = E5Wrapper(base)
    wrapper.eval()

    dummy_text = "exporting to onnx " * 64
    enc = tok(dummy_text, return_tensors="pt", padding="max_length",
              truncation=True, max_length=seq_len)
    input_ids = enc["input_ids"].to(torch.int64)
    attn = enc["attention_mask"].to(torch.int64)

    out_path.parent.mkdir(parents=True, exist_ok=True)
    # E5-large FP32 weights are ~2.2 GB, past protobuf's 2 GB single-file
    # limit. Force external-data format so weights land in a sibling .data
    # file.
    torch.onnx.export(
        wrapper,
        (input_ids, attn),
        out_path.as_posix(),
        input_names=["input_ids", "attention_mask"],
        output_names=["sentence_embedding"],
        dynamic_axes=None,
        opset_version=17,
    )
    # Re-save with external data to make downstream tools (quark, onnxsim)
    # happy. torch.onnx.export's own external-data emission is inconsistent
    # across versions.
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


def quantize(in_path: Path, out_path: Path, preset: str,
             bf16_shrink: bool = True) -> None:
    from quark.onnx import ModelQuantizer
    from quark.onnx.quantization.config import Config, get_default_config

    # Quark's `check_onnx_model(float_model)` (quantize.py:251) calls
    # `create_infer_session_for_onnx_model` without forwarding
    # `use_external_data_format`, so for a 2 GB+ FP32 model it tries to
    # serialize the ModelProto inline and bombs with
    # `google.protobuf.message.EncodeError: Failed to serialize proto`.
    # Patch the check to skip the round-trip ByteSize/serialise on large
    # models — quantize_static still does its own load below.
    # Patch `create_infer_session_for_onnx_model` (used in several call
    # sites that don't forward `use_external_data_format`) so it always
    # opens >2 GB models with external-data refs.
    import quark.onnx.utils.model_utils as _mu
    import quark.onnx.quantization.quantize as _qz
    import quark.onnx.calibration.data_readers as _dr
    _orig_make = _mu.create_infer_session_for_onnx_model

    def _make_session(model_input, sess_options=None, use_external_data_format=True,
                      *args, **kwargs):
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
    # >2 GB FP32 weights → external data is mandatory.
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
    """Quark's BF16-QDQ-to-Cast output keeps weight initializers as FP32 and
    adds Cast(FP32→BF16) nodes in front of every matmul. For models past 2
    GiB total that breaks: VitisAI EP serialises the ModelProto into a
    string buffer during compilation, and protobuf's 2 GiB single-message
    cap kicks in.

    Rewrite every FP32 initializer that feeds *only* a Cast-to-BF16 to
    store its data as BFLOAT16 in raw_data. The downstream Cast op already
    expects BF16 input semantics, so quality is preserved (you'd otherwise
    cast FP32→BF16 at runtime anyway) and the on-disk weight bytes halve."""
    import numpy as np
    import onnx
    from onnx import TensorProto, numpy_helper

    model = onnx.load(path.as_posix(), load_external_data=True)
    g = model.graph

    # Map: initializer name → set of (node_idx, input_idx) consumers.
    consumers: dict[str, list[tuple[int, int, "onnx.NodeProto"]]] = {}
    for ni, node in enumerate(g.node):
        for ii, name in enumerate(node.input):
            consumers.setdefault(name, []).append((ni, ii, node))

    converted = 0
    skipped_shared = 0
    for init in list(g.initializer):
        if init.data_type != TensorProto.FLOAT:
            continue
        consumes = consumers.get(init.name, [])
        if not consumes:
            continue
        # Only convert if every consumer is a Cast op whose `to` attr is
        # BFLOAT16. That way we don't change semantics for any op that
        # genuinely needs FP32 input.
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
            skipped_shared += 1
            continue

        arr_fp32 = numpy_helper.to_array(init).astype(np.float32, copy=False)
        # Round-to-nearest-even FP32 → BF16 via int32 view.
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
        # The downstream Cast is now redundant (BF16 → BF16). Replace
        # each Cast's output usage with the initializer directly.
        for ni, _, node in consumes:
            cast_out = node.output[0]
            new_in = init.name
            for other in g.node:
                for k, name in enumerate(other.input):
                    if name == cast_out:
                        other.input[k] = new_in
            node.op_type = "Identity"
            # Drop the now-bogus `to` attribute.
            attrs_keep = [a for a in node.attribute if a.name != "to"]
            del node.attribute[:]
            node.attribute.extend(attrs_keep)
        converted += 1

    print(f"  bf16-shrink: converted {converted} initializers "
          f"(skipped {skipped_shared} non-Cast-fed)", file=sys.stderr)

    # Save with one consolidated external-data file.
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
    import numpy as np
    import onnxruntime as ort

    cache_dir.mkdir(parents=True, exist_ok=True)
    so = ort.SessionOptions()
    so.log_severity_level = 2  # Warnings+
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

    # One-shot dry run to verify it actually executes.
    seq_len = sess.get_inputs()[0].shape[1]
    ids = np.zeros((1, seq_len), dtype=np.int64)
    mask = np.ones((1, seq_len), dtype=np.int64)
    t0 = time.time()
    out = sess.run(None, {"input_ids": ids, "attention_mask": mask})
    inference_ms = round((time.time() - t0) * 1000, 2)

    return {
        "compile_seconds": compile_seconds,
        "first_inference_ms": inference_ms,
        "embedding_dim": out[0].shape[-1],
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--model-id", default="intfloat/multilingual-e5-base")
    ap.add_argument("--seq-len", type=int, default=512)
    ap.add_argument("--quant-preset", default="INT8_TRANSFORMER_DEFAULT",
                    help="Quark preset (BF16, INT8_TRANSFORMER_DEFAULT, "
                         "INT8_TRANSFORMER_ACCURATE, XINT8, MX9_INT8, ...)")
    ap.add_argument("--output-dir", type=Path,
                    default=Path.home() / ".cache/sy/npu-embed")
    ap.add_argument("--vaip-config", type=Path,
                    default=Path("/opt/AMD/ryzenai/venv/lib/python3.12/"
                                 "site-packages/voe-4.0-linux_x86_64/"
                                 "vaip_config.json"))
    ap.add_argument("--skip-warm", action="store_true",
                    help="skip the NPU compile/warmup step")
    ap.add_argument("--no-bf16-shrink", action="store_true",
                    help="Skip the FP32→BF16 initializer shrink pass. Needed "
                         "for models small enough to fit under the 2 GiB "
                         "protobuf cap (e.g. multilingual-e5-base, ~580 MB "
                         "BF16); the shrink can leave dangling QDQ edges "
                         "that VitisAI's partition pass rejects.")
    ap.add_argument("--json", action="store_true",
                    help="emit a final JSON summary on stdout")
    args = ap.parse_args()

    if not args.vaip_config.is_file():
        print(f"error: vaip config not found: {args.vaip_config}",
              file=sys.stderr)
        print("hint: did you `source /opt/AMD/ryzenai/venv/bin/activate`?",
              file=sys.stderr)
        return 2

    out_dir = args.output_dir.expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)
    model_stem = args.model_id.split("/")[-1]
    fp32 = out_dir / f"{model_stem}.onnx"
    suffix = args.quant_preset.lower()
    quant = out_dir / f"{model_stem}.{suffix}.onnx"
    cache_dir = out_dir
    cache_key = (f"compiled_{model_stem}_"
                 f"{suffix}_seq{args.seq_len}")

    summary: dict = {"model_id": args.model_id, "seq_len": args.seq_len,
                     "output_dir": str(out_dir)}

    if not fp32.is_file():
        print(f"[1/3] Exporting {args.model_id} → ONNX (seq_len={args.seq_len})",
              file=sys.stderr)
        export_onnx(args.model_id, args.seq_len, fp32)
    else:
        print(f"[1/3] Reusing existing {fp32.name}", file=sys.stderr)
    summary["fp32_path"] = str(fp32)
    summary["fp32_bytes"] = fp32.stat().st_size

    if not quant.is_file():
        print(f"[2/3] Quantising → {args.quant_preset}", file=sys.stderr)
        quantize(fp32, quant, args.quant_preset,
                 bf16_shrink=not args.no_bf16_shrink)
    else:
        print(f"[2/3] Reusing existing {quant.name}", file=sys.stderr)
    summary["quant_preset"] = args.quant_preset
    summary["quant_path"] = str(quant)
    summary["quant_bytes"] = quant.stat().st_size

    if args.skip_warm:
        print(f"[3/3] Skipping NPU warm (--skip-warm)", file=sys.stderr)
    else:
        print(f"[3/3] Compiling on NPU (~3 min on first run)", file=sys.stderr)
        summary.update(warm_npu_cache(quant, args.vaip_config, cache_dir, cache_key))
        summary["cache_dir"] = str(cache_dir / cache_key)

    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        for k, v in summary.items():
            print(f"  {k:24s} {v}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
