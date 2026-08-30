#!/usr/bin/env python3
"""Promote a source-verified cache artifact by hard link without copying blocks."""

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys

CHUNK_BYTES = 16 * 1024 * 1024
SCHEMA = "sy.spark.cache-promotion/v1"
TRANSFORM = "ordered-safetensors-tensor-concatenation/v1"
DTYPE_BYTES = {
    "BOOL": 1,
    "F8_E4M3": 1,
    "F8_E5M2": 1,
    "I8": 1,
    "U8": 1,
    "BF16": 2,
    "F16": 2,
    "I16": 2,
    "U16": 2,
    "F32": 4,
    "I32": 4,
    "U32": 4,
    "F64": 8,
    "I64": 8,
    "U64": 8,
}


def load_json(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def confined(root, relative):
    root = Path(root).resolve(strict=True)
    path = (root / relative).resolve(strict=True)
    if path != root and root not in path.parents:
        raise ValueError("model path escapes its declared root")
    return path


def regular_file(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    metadata = os.fstat(descriptor)
    if not stat.S_ISREG(metadata.st_mode):
        os.close(descriptor)
        raise ValueError("cache artifact is not a regular file")
    return descriptor, metadata


def stream_range(handle, offset, length, digest):
    handle.seek(offset)
    remaining = length
    while remaining:
        chunk = handle.read(min(CHUNK_BYTES, remaining))
        if not chunk:
            raise ValueError("safetensors data is truncated")
        digest.update(chunk)
        remaining -= len(chunk)
    os.posix_fadvise(handle.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)


def safetensors_header(path):
    with open(path, "rb") as handle:
        prefix = handle.read(8)
        if len(prefix) != 8:
            raise ValueError("safetensors header is truncated")
        length = int.from_bytes(prefix, "little")
        if length <= 0 or length > 64 * 1024 * 1024:
            raise ValueError("safetensors header length is invalid")
        payload = handle.read(length)
        if len(payload) != length:
            raise ValueError("safetensors header is truncated")
    header = json.loads(payload)
    if not isinstance(header, dict):
        raise ValueError("safetensors header is invalid")
    return 8 + length, header


def tensor_digest(contract):
    root = Path(contract["model_root"]).resolve(strict=True)
    index_relative = Path(contract["model_index"])
    index_path = confined(root, index_relative)
    index = load_json(index_path)
    weight_map = index.get("weight_map")
    if not isinstance(weight_map, dict):
        raise ValueError("model index has no weight map")
    pattern = re.compile(contract["tensor_pattern"])
    selected = []
    for name, filename in weight_map.items():
        match = pattern.fullmatch(name)
        if match:
            selected.append((int(match.group(1)), name, filename))
    selected.sort()
    if [index for index, _, _ in selected] != list(range(len(selected))) or not selected:
        raise ValueError("selected tensor indices are not contiguous")
    digest = hashlib.sha256()
    total = 0
    headers = {}
    for _, name, filename in selected:
        path = confined(root, index_relative.parent / filename)
        if path not in headers:
            headers[path] = safetensors_header(path)
        data_start, header = headers[path]
        tensor = header.get(name)
        if not isinstance(tensor, dict):
            raise ValueError("indexed tensor is missing from its shard")
        dtype = tensor.get("dtype")
        shape = tensor.get("shape")
        offsets = tensor.get("data_offsets")
        if dtype not in DTYPE_BYTES or not isinstance(shape, list):
            raise ValueError("tensor metadata is unsupported")
        if not isinstance(offsets, list) or len(offsets) != 2:
            raise ValueError("tensor offsets are invalid")
        start, end = offsets
        elements = 1
        for dimension in shape:
            if not isinstance(dimension, int) or dimension < 0:
                raise ValueError("tensor shape is invalid")
            elements *= dimension
        length = end - start
        if start < 0 or length != elements * DTYPE_BYTES[dtype]:
            raise ValueError("tensor byte length is invalid")
        with open(path, "rb") as handle:
            stream_range(handle, data_start + start, length, digest)
        total += length
    return digest.hexdigest(), total


def file_digest(descriptor):
    digest = hashlib.sha256()
    with os.fdopen(os.dup(descriptor), "rb") as handle:
        for chunk in iter(lambda: handle.read(CHUNK_BYTES), b""):
            digest.update(chunk)
        os.posix_fadvise(handle.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    return digest.hexdigest()


def validate(contract):
    required = {
        "schema",
        "model_root",
        "model_index",
        "tensor_pattern",
        "content_transform",
        "model_identity",
        "source_artifact",
        "source_marker",
        "destination_root",
        "admission_report",
        "destination_subdirectory",
        "legacy_marker",
        "target_marker",
    }
    if set(contract) != required or contract["schema"] != SCHEMA:
        raise ValueError("cache promotion contract is invalid")
    if contract["content_transform"] != TRANSFORM:
        raise ValueError("cache content transform is unsupported")
    admission = load_json(contract["admission_report"])
    selection = admission.get("selection")
    if admission.get("schema") != "sy.spark.admission-report/v1" or not isinstance(selection, dict):
        raise ValueError("admission report has no authoritative cache namespace")
    namespace = selection.get("compile_cache_namespace")
    if not isinstance(namespace, str):
        raise ValueError("admission report has no authoritative cache namespace")
    legacy = contract["legacy_marker"]
    target = contract["target_marker"]
    if load_json(contract["source_marker"]) != legacy:
        raise ValueError("legacy marker does not match the promotion contract")
    shared = ("shape", "dtype", "bytes")
    if any(legacy.get(key) != target.get(key) for key in shared):
        raise ValueError("cache marker tensor identity changed")
    source = "sha256:" + hashlib.sha256(contract["model_identity"].encode()).hexdigest()
    if target.get("source") != source:
        raise ValueError("target marker has the wrong model source identity")
    sha256 = target.get("sha256")
    if not isinstance(sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", sha256):
        raise ValueError("target marker has no content hash")
    descriptor, metadata = regular_file(contract["source_artifact"])
    try:
        if metadata.st_size != target["bytes"]:
            raise ValueError("cache artifact byte count changed")
        tensor_sha256, tensor_bytes = tensor_digest(contract)
        artifact_sha256 = file_digest(descriptor)
        if tensor_bytes != metadata.st_size or tensor_sha256 != sha256 or artifact_sha256 != sha256:
            raise ValueError("cache artifact does not match the immutable source tensors")
        return descriptor, metadata, namespace
    except BaseException:
        os.close(descriptor)
        raise


def confined_destination(root, relative):
    relative = Path(relative)
    if relative.is_absolute() or not relative.parts or any(part in (".", "..") for part in relative.parts):
        raise ValueError("cache destination path is invalid")
    return root.joinpath(relative)


def create_owned_tree(root, relative, owner):
    destination = root
    for part in Path(relative).parts:
        destination /= part
        try:
            os.mkdir(destination, 0o770)
        except FileExistsError:
            metadata = os.stat(destination, follow_symlinks=False)
            if not stat.S_ISDIR(metadata.st_mode):
                raise ValueError("cache destination component is not a directory")
        os.chown(destination, owner.st_uid, owner.st_gid)
        os.chmod(destination, 0o770)
    return destination


def publish(contract, descriptor, metadata, namespace_relative):
    root = Path(contract["destination_root"]).resolve(strict=True)
    owner = os.stat(root, follow_symlinks=False)
    if not stat.S_ISDIR(owner.st_mode):
        raise ValueError("cache destination root is not a directory")
    namespace = confined_destination(root, namespace_relative)
    destination = confined_destination(namespace, contract["destination_subdirectory"])
    create_owned_tree(root, namespace_relative, owner)
    create_owned_tree(namespace, contract["destination_subdirectory"], owner)
    name = Path(contract["source_artifact"]).name
    artifact = destination / name
    temporary = destination / (name + ".promoting")
    marker = destination / (name + ".complete.json")
    marker_temporary = destination / (name + ".complete.json.promoting")
    try:
        os.link(contract["source_artifact"], temporary, follow_symlinks=False)
        os.fchmod(descriptor, 0o440)
        os.replace(temporary, artifact)
        payload = json.dumps(contract["target_marker"], sort_keys=True, separators=(",", ":")) + "\n"
        marker_descriptor = os.open(marker_temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o440)
        with os.fdopen(marker_descriptor, "w", encoding="utf-8") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.chown(marker_temporary, metadata.st_uid, metadata.st_gid)
        os.replace(marker_temporary, marker)
        directory_descriptor = os.open(destination, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_descriptor)
        finally:
            os.close(directory_descriptor)
    except BaseException:
        for path in (temporary, marker_temporary):
            try:
                path.unlink()
            except FileNotFoundError:
                pass
        raise
    finally:
        os.close(descriptor)


def main():
    if len(sys.argv) != 2:
        raise ValueError("usage: promote-spark-cache.py CONTRACT.json")
    contract = load_json(sys.argv[1])
    descriptor, metadata, namespace = validate(contract)
    publish(contract, descriptor, metadata, namespace)
    print("cache promotion verified and published without copying blocks")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
