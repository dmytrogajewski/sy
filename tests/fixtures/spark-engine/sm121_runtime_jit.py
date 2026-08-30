"""Exercise the runtime CUDA JITs required by an SM121 Spark engine image."""

import json

import torch
import triton
import triton.language as tl


@triton.jit
def _triton_add(left, right, output, elements, block_size: tl.constexpr):
    offsets = tl.program_id(0) * block_size + tl.arange(0, block_size)
    mask = offsets < elements
    tl.store(output + offsets, tl.load(left + offsets, mask=mask) + tl.load(right + offsets, mask=mask), mask=mask)


@torch.compile(fullgraph=True)
def _compiled_add(left, right):
    return left + right


def _tilelang_add(left, right):
    import tilelang
    import tilelang.language as T

    @tilelang.jit
    def kernel(
        source,
        operand,
        block_m: int = 16,
        block_n: int = 16,
        threads: int = 128,
    ):
        rows, columns = T.const("rows, columns")
        source: T.Tensor((rows, columns), T.float32)
        operand: T.Tensor((rows, columns), T.float32)
        result = T.empty((rows, columns), T.float32)
        with T.Kernel(
            T.ceildiv(columns, block_n), T.ceildiv(rows, block_m), threads=threads
        ) as (column_block, row_block):
            source_shared = T.alloc_shared((block_m, block_n), T.float32)
            operand_shared = T.alloc_shared((block_m, block_n), T.float32)
            result_local = T.alloc_fragment((block_m, block_n), T.float32)
            result_shared = T.alloc_shared((block_m, block_n), T.float32)
            T.copy(
                source[
                    row_block * block_m,
                    column_block * block_n,
                ],
                source_shared,
            )
            T.copy(
                operand[
                    row_block * block_m,
                    column_block * block_n,
                ],
                operand_shared,
            )
            for row, column in T.Parallel(block_m, block_n):
                result_local[row, column] = (
                    source_shared[row, column] + operand_shared[row, column]
                )
            T.copy(result_local, result_shared)
            T.copy(
                result_shared,
                result[
                    row_block * block_m,
                    column_block * block_n,
                ],
            )
        return result

    return kernel(left, right)


def main():
    capability = torch.cuda.get_device_capability()
    assert capability == (12, 1), capability

    left = torch.arange(256, device="cuda", dtype=torch.float32)
    right = torch.full_like(left, 2.0)
    expected = left + right

    triton_output = torch.empty_like(left)
    _triton_add[(triton.cdiv(left.numel(), 64),)](
        left, right, triton_output, left.numel(), block_size=64
    )
    torch.testing.assert_close(triton_output, expected)
    torch.testing.assert_close(_compiled_add(left, right), expected)

    tilelang_output = _tilelang_add(left.reshape(16, 16), right.reshape(16, 16))
    torch.testing.assert_close(tilelang_output, expected.reshape(16, 16))

    print(
        json.dumps(
            {
                "capability": list(capability),
                "device": torch.cuda.get_device_name(),
                "tilelang": "ok",
                "torch_compile": "ok",
                "triton": "ok",
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
