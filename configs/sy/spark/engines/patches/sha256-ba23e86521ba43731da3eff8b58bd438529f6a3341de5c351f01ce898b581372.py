#!/usr/bin/env python3
"""Exercise ordering and failure semantics of the post-warmup flush asset."""

import asyncio
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace


SOURCE = '''
from __future__ import annotations
import logging
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    TokenizerManager = object
logger = logging.getLogger(__name__)
registry = {}
events = []
def warmup(name):
    def register(function):
        registry[name] = function
        return function
    return register
@warmup("prefill_shapes")
async def prefill_shapes(disaggregation_mode, tokenizer_manager):
    for shape in (64, 32768):
        events.append(("shape", shape))
        generate_req_input = shape
        await tokenizer_manager.generate_request(generate_req_input, None).__anext__()
'''


class Manager:
    def __init__(self, success: bool):
        self.success = success
        self.idle = True

    async def generate_request(self, shape, _request):
        self.idle = False
        self.idle = True
        yield shape

    async def flush_cache(self, timeout_s):
        assert self.idle
        assert events == [("shape", 64), ("shape", 32768)]
        events.append(("flush", timeout_s))
        return SimpleNamespace(success=self.success, message="busy")


async def exercise(namespace, success: bool):
    events.clear()
    manager = Manager(success)
    await namespace["registry"]["prefill_shapes"]("null", manager)
    await namespace["registry"]["flush_transient_allocator"]("null", manager)


def main() -> None:
    with tempfile.TemporaryDirectory() as directory:
        target = Path(directory) / "warmup.py"
        target.write_text(SOURCE, encoding="utf-8")
        subprocess.run([sys.executable, sys.argv[1], target], check=True)
        patched = target.read_text(encoding="utf-8")
        assert not any(token in patched for token in ("drop_caches", "posix_fadvise"))
        namespace = {"__name__": "warmup_contract"}
        exec(compile(patched, target, "exec"), namespace)
        globals()["events"] = namespace["events"]
        asyncio.run(exercise(namespace, True))
        assert namespace["events"][-1] == ("flush", 30.0)
        try:
            asyncio.run(exercise(namespace, False))
        except RuntimeError as error:
            assert "Post-warmup cache flush failed" in str(error)
        else:
            raise AssertionError("failed typed flush did not fail closed")
    print("post-warmup ordered idle allocator flush contract: ok")


if __name__ == "__main__":
    main()
