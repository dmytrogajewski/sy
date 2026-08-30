#!/usr/bin/env python3
"""Validate the patched PLE allocator and preserved gather contract."""

import ast
import os
import sys
import tempfile
from unittest import mock

import torch


ROWS = 5
WIDTH = 4
VOCAB_START = 11


def only_named(nodes, name):
    matches = [node for node in nodes if getattr(node, "name", None) == name]
    assert len(matches) == 1, f"expected exactly one {name}, found {len(matches)}"
    return matches[0]


def load_allocator(source, tree):
    functions = {
        "_alloc_ple_table",
        "_ple_source_identity",
        "_sha256_path",
        "_clean_ple_temporaries",
        "_ple_cache_state",
    }
    nodes = [
        node for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name in functions
        or isinstance(node, ast.Assign)
        and any(
            isinstance(target, ast.Name) and target.id.startswith("_PLE_")
            for target in node.targets
        )
    ]
    namespace = {"torch": torch}
    exec(compile(ast.Module(nodes, []), "<ple-allocator>", "exec"), namespace)
    return namespace


def assert_upstream_pointer_path(source, tree):
    embedding = only_named(tree.body, "Qwen4ExpPinnedHostEmbedding")
    initializer = only_named(embedding.body, "__init__")
    calls = [
        node
        for node in ast.walk(initializer)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "_alloc_ple_table"
    ]
    assert len(calls) == 1, f"expected one mmap allocator call, found {len(calls)}"
    required = (
        "_gather_ple_embedding_from_pinned_kernel",
        "self.weight.data_ptr()",
        "self._prefetch_stream",
        "self._graph_prefetch_buffers",
        "wait_stream(self._prefetch_stream)",
    )
    assert all(anchor in source for anchor in required), "upstream gather path changed"


def reference_gather(weight, ids):
    in_range = (ids >= VOCAB_START) & (ids < VOCAB_START + ROWS)
    output = torch.zeros((*ids.shape, WIDTH), dtype=torch.bfloat16)
    local_ids = (ids[in_range] - VOCAB_START).long()
    raw_rows = weight.view(torch.uint8)[local_ids]
    output[in_range] = raw_rows.view(torch.float8_e4m3fn).to(torch.bfloat16)
    return output


def assert_mmap_math(namespace):
    with tempfile.TemporaryDirectory() as directory:
        os.environ["SGLANG_QWEN4_PLE_MMAP_DIR"] = directory
        os.environ["SGLANG_QWEN4_PLE_SOURCE_ID"] = "synthetic-checkpoint"
        namespace["_PLE_MMAP_DIR"] = None
        table = namespace["_alloc_ple_table"]((ROWS, WIDTH), torch.float8_e4m3fn)
        path = namespace["_PLE_CACHE_ALLOCATION_STATE"]["temporary"]
        assert tuple(table.shape) == (ROWS, WIDTH) and os.path.getsize(path) == ROWS * WIDTH

        source = torch.tensor(
            [[0.5, 1.0, 2.0, 3.0], [-0.5, -1.0, -2.0, -3.0],
             [4.0, 5.0, 6.0, 7.0], [0.25, 0.75, 1.5, 2.5],
             [-4.0, -5.0, -6.0, -7.0]],
            dtype=torch.float32,
        ).to(torch.float8_e4m3fn)
        table.copy_(source)
        raw = torch.from_file(path, shared=True, size=ROWS * WIDTH, dtype=torch.uint8)
        reopened = raw.view(torch.float8_e4m3fn).view(ROWS, WIDTH)
        assert torch.equal(reopened.view(torch.uint8), source.view(torch.uint8))
        assert torch.equal(reference_gather(reopened, torch.tensor([VOCAB_START + 2]))[0], source[2].to(torch.bfloat16))

        batch_ids = torch.tensor([[VOCAB_START, VOCAB_START + 4], [VOCAB_START + 1, VOCAB_START + 3]])
        batch_raw = source.view(torch.uint8)[batch_ids - VOCAB_START]
        expected_batch = batch_raw.view(torch.float8_e4m3fn).to(torch.bfloat16)
        assert torch.equal(reference_gather(reopened, batch_ids), expected_batch)
        outside = reference_gather(reopened, torch.tensor([VOCAB_START - 1, VOCAB_START + ROWS]))
        assert torch.count_nonzero(outside) == 0
        with mock.patch("ctypes.CDLL", side_effect=OSError("expected advice failure")):
            unadvised = namespace["_alloc_ple_table"]((1, 1), torch.float8_e4m3fn)
        assert tuple(unadvised.shape) == (1, 1)


def main(path):
    with open(path, encoding="utf-8") as handle:
        source = handle.read()
    tree = ast.parse(source)
    namespace = load_allocator(source, tree)
    assert_upstream_pointer_path(source, tree)
    assert_mmap_math(namespace)
    print("PLE mmap AST, write, view, range, batch, and FP8 contract: ok")


if __name__ == "__main__":
    main(sys.argv[1])
