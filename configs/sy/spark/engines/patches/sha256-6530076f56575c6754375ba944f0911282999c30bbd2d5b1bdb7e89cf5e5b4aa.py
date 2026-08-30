#!/usr/bin/env python3
"""Keep verified PLE scans from occupying the serving page-cache budget."""

import sys


OLD = '''def _sha256_path(path):
    import hashlib

    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(16 * 1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()
'''

NEW = '''def _sha256_path(path):
    import hashlib
    import logging
    import os

    digest = hashlib.sha256()
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        try:
            os.posix_fadvise(descriptor, 0, 0, os.POSIX_FADV_NOREUSE)
        except OSError as error:
            logging.getLogger(__name__).warning("PLE cache scan advice failed (%s)", error)
        with os.fdopen(descriptor, "rb", closefd=False) as handle:
            for chunk in iter(lambda: handle.read(16 * 1024 * 1024), b""):
                digest.update(chunk)
        try:
            os.posix_fadvise(descriptor, 0, 0, os.POSIX_FADV_DONTNEED)
        except OSError as error:
            logging.getLogger(__name__).warning("PLE cache reclaim advice failed (%s)", error)
    finally:
        os.close(descriptor)
    return digest.hexdigest()
'''


def main(path):
    with open(path, encoding="utf-8") as handle:
        source = handle.read()
    if "PLE cache reclaim advice failed" in source:
        print("ALREADY PATCHED:", path)
        return
    if source.count(OLD) != 1:
        raise ValueError("expected one PLE checksum anchor")
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(source.replace(OLD, NEW, 1))
    print("PATCHED:", path)


if __name__ == "__main__":
    try:
        main(sys.argv[1])
    except (IndexError, OSError, ValueError) as error:
        print("ERROR:", error)
        sys.exit(1)
