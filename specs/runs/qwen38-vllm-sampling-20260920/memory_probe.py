"""Bounded first-use CUDA probe; run only with the managed model stopped."""
import argparse
import json
from pathlib import Path

import torch
from vllm.v1.sample.ops.topk_topp_sampler import apply_top_k_top_p

GIB = 1024 ** 3
HOST_RESERVE = 16 * GIB
CHUNK = 2 * GIB
parser = argparse.ArgumentParser()
parser.add_argument('--reserve-gib', type=int, choices=range(0, 93), default=0)
args = parser.parse_args()


def host_available():
    return next(int(line.split()[1]) * 1024 for line in
                Path('/proc/meminfo').read_text().splitlines()
                if line.startswith('MemAvailable:'))


def mark(stage):
    torch.cuda.synchronize()
    free, total = torch.cuda.mem_get_info()
    print(json.dumps({'stage': stage, 'cuda_free': free, 'cuda_total': total,
                      'allocated': torch.cuda.memory_allocated(),
                      'host_available': host_available()}), flush=True)


logits = torch.randn(32, 248320, device='cuda', dtype=torch.float32)
k = torch.full((32,), 20, device='cuda', dtype=torch.int32)
p = torch.full((32,), 0.95, device='cuda', dtype=torch.float32)
apply_top_k_top_p(logits.clone(), k, p)
x = torch.randn(8192, 2560, device='cuda', dtype=torch.bfloat16)
weight = torch.randn(1, 2560, device='cuda', dtype=torch.bfloat16)
torch.nn.functional.linear(x, weight)
mark('ordinary-warmup')
reserved = []
for offset in range(0, args.reserve_gib * GIB, CHUNK):
    amount = min(CHUNK, args.reserve_gib * GIB - offset)
    if host_available() - amount < HOST_RESERVE:
        raise RuntimeError('Refusing to cross the 16-GiB host memory reserve')
    reserved.append(torch.zeros(amount, device='cuda', dtype=torch.uint8))
    mark('reserved-' + str(offset + amount))

stream = torch.cuda.Stream()
stream.wait_stream(torch.cuda.current_stream())
with torch.cuda.stream(stream):
    for _ in range(3):
        torch.nn.functional.linear(x[:32], weight)
torch.cuda.current_stream().wait_stream(stream)
graph = torch.cuda.CUDAGraph()
with torch.cuda.graph(graph):
    graph_output = torch.nn.functional.linear(x[:32], weight)
graph.replay()
mark('graph-replay')
apply_top_k_top_p(logits[:8].clone(), k[:8], p[:8])
mark('first-eight-row-sampler')
torch.argsort(torch.tensor([1, 0], device='cuda'), stable=True)
mark('first-stable-sort')
for rows in (1, 4, 8, 16, 32, 64, 8000, 8004, 8008, 8012, 8191, 8192):
    result = torch.nn.functional.linear(x[:rows], weight)
    assert torch.isfinite(result).all().item()
    mark('gemm-' + str(rows))
print('PASS: first-use sampler, stable sort, and BF16 GEMM after graph replay', flush=True)
