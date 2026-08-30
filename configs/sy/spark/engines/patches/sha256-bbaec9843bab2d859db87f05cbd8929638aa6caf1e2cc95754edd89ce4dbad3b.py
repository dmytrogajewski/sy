#!/usr/bin/env python3
"""Exercise durable PLE publication and recovery with a synthetic FP8 table."""

import ast
import json
import logging
import os
import stat
import sys
import tempfile
from types import SimpleNamespace

import torch

ROWS = 7
WIDTH = 5
SHARDS = 2


def load_contract(path):
    tree = ast.parse(open(path, encoding="utf-8").read())
    functions = {"_ple_source_identity", "_sha256_path", "_clean_ple_temporaries",
                 "_ple_cache_state", "_take_ple_cache_state", "_finish_ple_cache",
                 "_alloc_ple_table"}
    wanted = {"_PLE_MMAP_DIR", "_PLE_CACHE_SCHEMA", "_PLE_CACHE_TRANSFORM",
              "_PLE_CACHE_ALLOCATION_STATE"}
    nodes = [node for node in tree.body if
             isinstance(node, ast.FunctionDef) and
             node.name in functions
             or isinstance(node, ast.Assign) and
             any(isinstance(target, ast.Name) and target.id in wanted for target in node.targets)]
    namespace = {"torch": torch, "__name__": "ple_persistence_self_test"}
    exec(compile(ast.Module(nodes, []), path, "exec"), namespace)
    return namespace


def reset(module, directory, source):
    os.environ["SGLANG_QWEN4_PLE_MMAP_DIR"] = directory
    os.environ["SGLANG_QWEN4_PLE_SOURCE_ID"] = source
    module["_PLE_MMAP_DIR"] = None
    module["_PLE_CACHE_ALLOCATION_STATE"] = None


def allocate(module):
    table = module["_alloc_ple_table"]((ROWS, WIDTH), torch.float8_e4m3fn)
    state = module["_take_ple_cache_state"]()
    assert state is not None
    return table, state


def populate(module, table, state, values):
    table.copy_(values)
    embedding = SimpleNamespace(
        weight=table,
        _ple_cache_state=state,
        _ple_loaded_shards=set(range(SHARDS)),
    )
    module["_finish_ple_cache"](embedding, SHARDS)


def main():
    module = load_contract(sys.argv[1])
    values = torch.arange(ROWS * WIDTH, dtype=torch.float32).view(ROWS, WIDTH)
    values = (values / 8).to(torch.float8_e4m3fn)
    messages = []

    class Capture(logging.Handler):
        def emit(self, record):
            messages.append(record.getMessage())

    handler = Capture()
    logging.getLogger("ple_persistence_self_test").addHandler(handler)
    logging.getLogger("ple_persistence_self_test").setLevel(logging.INFO)
    with tempfile.TemporaryDirectory() as directory:
        source = "synthetic-checkpoint-revision"
        reset(module, directory, source)
        table, state = allocate(module)
        assert state["temporary"] and os.path.exists(state["temporary"])
        assert not os.path.exists(state["path"])
        populate(module, table, state, values)
        assert state["ready"] and state["temporary"] is None
        assert stat.S_IMODE(os.stat(state["path"]).st_mode) == 0o440
        with open(state["marker"], encoding="utf-8") as handle:
            marker = json.load(handle)
        assert marker["source"].startswith("sha256:") and len(marker["sha256"]) == 64

        reset(module, directory, source + "-different")
        foreign, foreign_state = allocate(module)
        assert not foreign_state["ready"] and foreign_state["temporary"]
        del foreign

        reset(module, directory, source)
        reused, reused_state = allocate(module)
        assert reused_state["ready"]
        before = torch.from_file(reused_state["path"], shared=False,
                                 size=ROWS * WIDTH, dtype=torch.uint8).clone()
        reused.view(torch.uint8)[0, 0] ^= 1
        after = torch.from_file(reused_state["path"], shared=False,
                                size=ROWS * WIDTH, dtype=torch.uint8)
        assert torch.equal(before, after)

        cancelled_dir = os.path.join(directory, "cancelled")
        reset(module, cancelled_dir, source)
        cancelled, cancelled_state = allocate(module)
        stale = cancelled_state["temporary"]
        del cancelled
        reset(module, cancelled_dir, source)
        replacement, replacement_state = allocate(module)
        assert not os.path.exists(stale) and os.path.exists(replacement_state["temporary"])
        del replacement

        os.chmod(reused_state["path"], 0o640)
        with open(reused_state["path"], "r+b") as handle:
            handle.write(b"\xff")
        os.chmod(reused_state["path"], 0o440)
        reset(module, directory, source)
        regenerated, regenerated_state = allocate(module)
        assert not regenerated_state["ready"] and regenerated_state["temporary"]
        populate(module, regenerated, regenerated_state, values)
        assert regenerated_state["ready"]

    joined = "\n".join(messages)
    assert all(word in joined for word in ("created", "verified", "reused", "rejected"))
    assert "synthetic-checkpoint-revision" not in joined
    print("PLE durable create, verify, reuse, cancellation, and regeneration contract: ok")


if __name__ == "__main__":
    main()
