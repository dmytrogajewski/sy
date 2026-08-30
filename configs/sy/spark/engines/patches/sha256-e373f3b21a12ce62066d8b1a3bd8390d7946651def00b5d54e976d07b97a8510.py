#!/usr/bin/env python3
"""Verify checksum integrity and bounded page-cache advice."""

import ast
import hashlib
import os
import sys
import tempfile
from unittest import mock


def load_checksum(path):
    tree = ast.parse(open(path, encoding="utf-8").read())
    functions = [
        node for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "_sha256_path"
    ]
    assert len(functions) == 1
    namespace = {"__name__": "ple_page_cache_self_test"}
    exec(compile(ast.Module(functions, []), path, "exec"), namespace)
    return namespace["_sha256_path"]


def main(path):
    checksum = load_checksum(path)
    payload = b"verified-ple-cache\n" * 1024
    with tempfile.NamedTemporaryFile() as artifact:
        artifact.write(payload)
        artifact.flush()
        with mock.patch("os.posix_fadvise") as advise:
            assert checksum(artifact.name) == hashlib.sha256(payload).hexdigest()
        advice = [call.args[3] for call in advise.call_args_list]
        assert advice == [os.POSIX_FADV_NOREUSE, os.POSIX_FADV_DONTNEED]
    print("PLE checksum page-cache reclaim contract: ok")


if __name__ == "__main__":
    main(sys.argv[1])
