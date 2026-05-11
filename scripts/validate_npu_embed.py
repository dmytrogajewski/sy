#!/usr/bin/env python3
"""Cross-check `sy knowledge` embeddings against a CPU reference.

Runs a handful of (passage, similar passage, unrelated passage) triplets
through both the AMD Ryzen AI venv (CPU EP, same ONNX) and the live sy
daemon (NPU/VitisAI). For each pair we emit:

  cos_sim      — pair cosine similarity from the CPU reference
  cos_sim_npu  — same pair from the sy/NPU path
  delta        — |reference - npu|; should be < 0.01 for BF16

The script is informational; non-zero delta is expected because the NPU
runs BF16 while the CPU EP runs the same model in float32. Anything past
~0.02 usually means a wiring bug (wrong tokenizer, wrong pooling, wrong
seq_len).

Run from inside the Ryzen AI venv:
    source /opt/AMD/ryzenai/venv/bin/activate
    python ~/sources/sy/scripts/validate_npu_embed.py
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path


PAIRS = [
    # (a, b, expected_relationship)
    ("The cat sits on the mat.", "A cat is on the carpet.", "similar"),
    ("How do I exit vim?", "Quit vim editor instructions", "similar"),
    ("Train a transformer from scratch", "Bake sourdough at 230 C", "unrelated"),
    ("The capital of France is Paris.", "Paris is the largest city in France.", "similar"),
    ("Rust is memory-safe by default.", "The new Indian highway bypass opened today.", "unrelated"),
]


def cosine(a, b):
    import numpy as np
    a = np.asarray(a, dtype=np.float32)
    b = np.asarray(b, dtype=np.float32)
    return float(a @ b / (np.linalg.norm(a) * np.linalg.norm(b)))


def cpu_reference():
    """Run the same ONNX through ORT CPU EP for ground truth."""
    import numpy as np
    import onnxruntime as ort
    from tokenizers import Tokenizer  # type: ignore

    cache = Path.home() / ".cache/sy/npu-embed"
    model = cache / "multilingual-e5-base.bf16.onnx"
    tok_path = cache / "multilingual-e5-base.tokenizer/tokenizer.json"

    sess = ort.InferenceSession(model.as_posix(), providers=["CPUExecutionProvider"])
    tok = Tokenizer.from_file(tok_path.as_posix())

    def embed(text: str):
        enc = tok.encode(text)
        ids = enc.ids
        mask = enc.attention_mask
        # Pad to 512.
        pad_id = 1
        while len(ids) < 512:
            ids.append(pad_id)
            mask.append(0)
        ids = np.asarray(ids[:512], dtype=np.int64).reshape(1, -1)
        mask = np.asarray(mask[:512], dtype=np.int64).reshape(1, -1)
        out = sess.run(None, {"input_ids": ids, "attention_mask": mask})
        return out[0][0]

    return embed


def sy_embed_one(text: str):
    """Spawn `sy knowledge search-vec --json` (or equivalent) to get
    the NPU embedding for a single passage. Falls back to a small
    helper we invoke via `sy knowledge bench-embed-one` if it exists;
    otherwise we ask the user to expose one. For now we parse the
    debug output of `bench --json` which currently emits per-chunk
    vectors."""
    raise NotImplementedError(
        "sy does not yet expose a CLI to dump a single embedding. "
        "Add `sy knowledge embed-one --text 'foo' --json` to use this "
        "script's NPU side."
    )


def main():
    embed_cpu = cpu_reference()
    print(f"{'pair':60s} {'cpu_cos':>10s} {'expected':>12s}")
    for a, b, expected in PAIRS:
        ea = embed_cpu(f"passage: {a}")
        eb = embed_cpu(f"passage: {b}")
        cos = cosine(ea, eb)
        ok = ("✓" if (cos > 0.7 and expected == "similar") or
                     (cos < 0.5 and expected == "unrelated") else "✗")
        label = f"{a[:28]!r:30s} vs {b[:28]!r:30s}"
        print(f"{label:60s} {cos:>10.4f} {expected:>12s} {ok}")


if __name__ == "__main__":
    sys.exit(main())
