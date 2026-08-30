#!/usr/bin/env python3
"""Add source-bound, crash-safe publication and warm reuse to the PLE mmap."""

import sys

HELPERS = r'''

_PLE_CACHE_SCHEMA = "sy.spark.ple-cache/v2"
_PLE_CACHE_TRANSFORM = "ple-mmap-persist-v2"
_PLE_CACHE_ALLOCATION_STATE = None


def _ple_source_identity():
    import hashlib
    import os
    import sys

    source = os.environ.get("SGLANG_QWEN4_PLE_SOURCE_ID", "").strip()
    if not source:
        for index, argument in enumerate(sys.argv[:-1]):
            if argument == "--model-path":
                source = sys.argv[index + 1]
                break
    if not source:
        raise RuntimeError("PLE cache requires an immutable model source identity")
    return "sha256:" + hashlib.sha256(source.encode()).hexdigest()


def _sha256_path(path):
    import hashlib

    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(16 * 1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _clean_ple_temporaries(path):
    import os

    directory, name = os.path.split(path)
    prefix = name + ".tmp-"
    try:
        entries = os.scandir(directory)
    except FileNotFoundError:
        return
    with entries:
        for entry in entries:
            if entry.name.startswith(prefix) and entry.is_file(follow_symlinks=False):
                try:
                    os.unlink(entry.path)
                except FileNotFoundError:
                    pass


def _ple_cache_state(shape, dtype):
    import json
    import logging
    import os
    import stat
    import time

    directory = os.environ.get("SGLANG_QWEN4_PLE_MMAP_DIR", "").strip()
    if not directory:
        return None
    numel = 1
    for dimension in shape:
        numel *= int(dimension)
    nbytes = numel * torch.empty(0, dtype=dtype).element_size()
    path = os.path.join(directory, "ple_table_%d_%d.bin" % (numel, nbytes))
    marker = path + ".complete.json"
    expected = {
        "schema": _PLE_CACHE_SCHEMA,
        "transform": _PLE_CACHE_TRANSFORM,
        "source": _ple_source_identity(),
        "shape": [int(dimension) for dimension in shape],
        "dtype": str(dtype),
        "bytes": nbytes,
    }
    ready = False
    artifact_present = os.path.lexists(path) or os.path.lexists(marker)
    try:
        with open(marker, encoding="utf-8") as handle:
            recorded = json.load(handle)
        checksum = recorded.pop("sha256")
        mode = os.stat(path, follow_symlinks=False).st_mode
        ready = (
            recorded == expected
            and stat.S_ISREG(mode)
            and mode & 0o222 == 0
            and os.path.getsize(path) == nbytes
            and _sha256_path(path) == checksum
        )
    except (FileNotFoundError, KeyError, OSError, ValueError, json.JSONDecodeError):
        pass
    if artifact_present and not ready:
        logging.getLogger(__name__).warning("PLE cache rejected: incomplete or corrupt artifact")
    os.makedirs(directory, exist_ok=True)
    _clean_ple_temporaries(path)
    _clean_ple_temporaries(marker)
    temporary = None if ready else "%s.tmp-%d-%d" % (path, os.getpid(), time.monotonic_ns())
    return {"path": path, "marker": marker, "temporary": temporary,
            "expected": expected, "ready": ready}


def _take_ple_cache_state():
    global _PLE_CACHE_ALLOCATION_STATE
    state = _PLE_CACHE_ALLOCATION_STATE
    _PLE_CACHE_ALLOCATION_STATE = None
    return state


def _finish_ple_cache(embedding, expected_shards):
    import ctypes
    import json
    import logging
    import os

    state = embedding._ple_cache_state
    if state is None or state["ready"]:
        return
    expected = set(range(expected_shards))
    if embedding._ple_loaded_shards != expected:
        raise RuntimeError(
            "PLE cache population is incomplete: loaded %d of %d shards"
            % (len(embedding._ple_loaded_shards), expected_shards)
        )
    temporary = state["temporary"]
    try:
        nbytes = state["expected"]["bytes"]
        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        if libc.msync(ctypes.c_void_p(embedding.weight.data_ptr()),
                     ctypes.c_size_t(nbytes), ctypes.c_int(4)) != 0:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error))
        data_fd = os.open(temporary, os.O_RDONLY)
        try:
            os.fsync(data_fd)
        finally:
            os.close(data_fd)
        payload = dict(state["expected"])
        payload["sha256"] = _sha256_path(temporary)
        logging.getLogger(__name__).info("PLE cache verified: complete temporary artifact")
        os.chmod(temporary, 0o440)
        os.replace(temporary, state["path"])
        marker_temporary = state["marker"] + ".tmp-%d" % os.getpid()
        with open(marker_temporary, "x", encoding="utf-8") as handle:
            json.dump(payload, handle, sort_keys=True, separators=(",", ":"))
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(marker_temporary, state["marker"])
        directory_fd = os.open(os.path.dirname(state["path"]), os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
        state["temporary"] = None
        state["ready"] = True
        logging.getLogger(__name__).info("PLE cache created: verified artifact published")
    except BaseException:
        for candidate in (temporary, state["marker"] + ".tmp-%d" % os.getpid()):
            try:
                os.unlink(candidate)
            except FileNotFoundError:
                pass
        logging.getLogger(__name__).warning("PLE cache rejected: population did not complete")
        raise

'''

CLASS_ANCHOR = "class Qwen4ExpPinnedHostEmbedding(VocabParallelEmbedding):"
REGISTER_ANCHOR = '        self.register_parameter("weight", cpu_weight)\n'
PATH_ANCHOR = '''    path = os.path.join(_PLE_MMAP_DIR, "ple_table_%d_%d.bin" % (numel, nbytes))
    if not os.path.exists(path) or os.path.getsize(path) != nbytes:
        with open(path, "wb") as f:
            f.truncate(nbytes)'''
STORAGE_ANCHOR = "    storage = torch.from_file(path, shared=True, size=nbytes, dtype=torch.uint8)"
EMBEDDING_ANCHOR = "            emb = ple_mod.ngram_embedding\n            if ("
COPY_ANCHOR = '''            copy_ple_rows_to_tp_embedding(emb, loaded_weight, shard_start, shard_end)
            loaded_shard_params.add(f"{mod_prefix}.ngram_embedding.weight")'''
FINALIZE_ANCHOR = '''        loaded_params.update(loaded_buffers)
        loaded_params.update(loaded_shard_params)'''


def replace_once(source: str, old: str, new: str, label: str) -> str:
    count = source.count(old)
    if count != 1:
        raise ValueError(f"expected one {label} anchor, found {count}")
    return source.replace(old, new, 1)


def main(path: str) -> int:
    with open(path, encoding="utf-8") as handle:
        source = handle.read()
    if '"sy.spark.ple-cache/v2"' in source:
        print("ALREADY PATCHED:", path)
        return 0
    source = replace_once(source, CLASS_ANCHOR, HELPERS.lstrip("\n") + "\n" + CLASS_ANCHOR, "class")
    source = replace_once(
        source, PATH_ANCHOR,
        '''    global _PLE_CACHE_ALLOCATION_STATE
    state = _ple_cache_state(shape, dtype)
    _PLE_CACHE_ALLOCATION_STATE = state
    path = state["path"] if state["ready"] else state["temporary"]
    if not state["ready"]:
        with open(path, "xb") as f:
            f.truncate(nbytes)
        logging.getLogger(__name__).info("PLE cache created: temporary artifact")''',
        "mmap path",
    )
    source = replace_once(
        source, STORAGE_ANCHOR,
        '    storage = torch.from_file(path, shared=not state["ready"], size=nbytes, dtype=torch.uint8)\n'
        '    if state["ready"]:\n'
        '        logging.getLogger(__name__).info("PLE cache reused: verified read-only artifact")',
        "mmap storage",
    )
    source = replace_once(
        source, REGISTER_ANCHOR,
        REGISTER_ANCHOR
        + "        self._ple_cache_state = _take_ple_cache_state()\n"
        + "        self._ple_loaded_shards = set()\n",
        "parameter registration",
    )
    source = replace_once(
        source, EMBEDDING_ANCHOR,
        '''            emb = ple_mod.ngram_embedding
            if isinstance(emb, Qwen4ExpPinnedHostEmbedding) and emb._ple_cache_state is not None and emb._ple_cache_state["ready"]:
                loaded_shard_params.add(f"{mod_prefix}.ngram_embedding.weight")
                return True
            if (''',
        "PLE embedding",
    )
    source = replace_once(
        source, COPY_ANCHOR,
        '''            copy_ple_rows_to_tp_embedding(emb, loaded_weight, shard_start, shard_end)
            if isinstance(emb, Qwen4ExpPinnedHostEmbedding):
                emb._ple_loaded_shards.add(shard_idx)
            loaded_shard_params.add(f"{mod_prefix}.ngram_embedding.weight")''',
        "PLE copy",
    )
    source = replace_once(
        source, FINALIZE_ANCHOR,
        '''        for ple_module in ple_modules.values():
            embedding = ple_module.ngram_embedding
            if isinstance(embedding, Qwen4ExpPinnedHostEmbedding) and embedding._ple_cache_state is not None and not embedding._ple_cache_state["ready"]:
                _finish_ple_cache(embedding, ple_num_sync_shards)

        loaded_params.update(loaded_buffers)
        loaded_params.update(loaded_shard_params)''',
        "load finalization",
    )
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(source)
    print("PATCHED:", path)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1]))
    except (IndexError, OSError, ValueError) as error:
        print("ERROR:", error)
        sys.exit(1)
