#!/usr/bin/env python3
"""Register a config-selected warmup that releases transient allocator state."""

from pathlib import Path
import sys


ANCHOR = '@warmup("prefill_shapes")'
ADDITION = '''

@warmup("flush_transient_allocator")
async def flush_transient_allocator(
    disaggregation_mode: str, tokenizer_manager: TokenizerManager
):
    """Release request state and unused device blocks after configured warmups."""
    result = await tokenizer_manager.flush_cache(timeout_s=30.0)
    if not result.success:
        raise RuntimeError(f"Post-warmup cache flush failed: {result.message}")
    logger.info("Post-warmup transient allocator state flushed")
'''


def main() -> None:
    path = Path(sys.argv[1])
    source = path.read_text(encoding="utf-8")
    if "def flush_transient_allocator(" in source:
        raise ValueError("post-warmup flush is already registered")
    if source.count(ANCHOR) != 1:
        raise ValueError("expected one prefill-shape warmup registration")
    if any(token in ADDITION for token in ("drop_caches", "posix_fadvise", "unlink(")):
        raise ValueError("post-warmup flush must remain allocator-local")
    path.write_text(source + ADDITION, encoding="utf-8")


if __name__ == "__main__":
    main()
