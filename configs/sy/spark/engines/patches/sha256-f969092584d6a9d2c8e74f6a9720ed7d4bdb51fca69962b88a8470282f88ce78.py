#!/usr/bin/env python3
"""Validate the SM120 sparse-decode import boundary."""
import sys

from sglang.srt.layers.attention.qwen_sparse_attn_backend import _resolve_trtllm_sparse_decode
from sglang.srt.utils import is_sm120_supported

assert callable(is_sm120_supported)
assert callable(_resolve_trtllm_sparse_decode)
assert "sglang.srt.layers.attention.trtllm_mha_backend" not in sys.modules
print("SM120 utility and QSA TRTLLM decode resolver import: ok")
