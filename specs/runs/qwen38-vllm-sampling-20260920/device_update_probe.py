"""Finite GPU lifecycle regression; run on Spark with the model stopped.

Uses only the pinned local image and removes its own exact containers.
No managed-engine labels, model mounts, host changes, or network access.
"""
import json
import subprocess
import time
import uuid

IMAGE = 'sha256:fa6008389ff17911099e649aeb84b225e71e529c448aa46814ef397dc557f5ff'
NODES = ['/dev/nvidia0', '/dev/nvidiactl', '/dev/nvidia-uvm', '/dev/nvidia-uvm-tools']
CUDA = '''import torch
from vllm.v1.sample.ops.topk_topp_sampler import apply_top_k_top_p
logits = torch.randn(32, 248320, device='cuda', dtype=torch.float32)
k = torch.full((32,), 20, device='cuda', dtype=torch.int32)
p = torch.full((32,), .95, device='cuda', dtype=torch.float32)
apply_top_k_top_p(logits.clone(), k, p)
torch.cuda.synchronize()
print('READY-FOR-UPDATE', flush=True)
input()
apply_top_k_top_p(logits[:8].clone(), k[:8], p[:8])
x = torch.randn(8004, 2560, device='cuda', dtype=torch.bfloat16)
w = torch.randn(1, 2560, device='cuda', dtype=torch.bfloat16)
assert torch.isfinite(torch.nn.functional.linear(x, w)).all().item()
torch.cuda.synchronize()
print('PASS-AFTER-UPDATE', flush=True)
'''


def docker(*args, check=True):
    return subprocess.run(['docker', *args], text=True, capture_output=True,
                          check=check, timeout=30)


for explicit in (False, True):
    name = 'sy-device-update-probe-' + uuid.uuid4().hex[:12]
    args = ['create', '--name', name, '-i', '--gpus', 'driver=nvidia,count=all',
            '--user', '65534', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
            '--read-only', '--network', 'none', '--memory', '4g', '--memory-swap', '4g',
            '--pids-limit', '128', '--tmpfs', '/tmp:rw,exec,size=1g', '--env', 'HOME=/tmp',
            '--env', 'TRITON_CACHE_DIR=/tmp/triton', '--entrypoint', 'python3']
    if explicit:
        for node in NODES:
            args += ['--device', node + ':' + node + ':rw']
    cid = docker(*args, IMAGE, '-u', '-c', CUDA).stdout.strip()
    print(json.dumps({'explicit_devices': explicit, 'container': cid}), flush=True)
    attached = None
    try:
        attached = subprocess.Popen(['docker', 'start', '-ai', cid], stdin=subprocess.PIPE,
                                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            logs = docker('logs', cid)
            if 'READY-FOR-UPDATE' in logs.stdout:
                break
            if attached.poll() is not None:
                raise RuntimeError('CUDA startup failed: ' + logs.stdout + logs.stderr)
            time.sleep(1)
        else:
            raise RuntimeError('CUDA startup deadline exceeded')
        before = docker('inspect', '--format', '{{.State.Pid}}', cid).stdout.strip()
        docker('exec', cid, 'nvidia-smi', '-L')
        docker('update', '--restart', 'unless-stopped', cid)
        after = docker('inspect', '--format', '{{.State.Pid}}', cid).stdout.strip()
        assert before == after
        nvml = docker('exec', cid, 'nvidia-smi', '-L', check=False)
        print(json.dumps({'nvml_exit': nvml.returncode,
                          'nvml_output': nvml.stdout + nvml.stderr}), flush=True)
        assert (nvml.returncode == 0) == explicit
        fresh = docker('exec', cid, 'python3', '-c',
                       'import torch; print(torch.ones(1, device="cuda"))', check=False)
        print(json.dumps({'fresh_cuda_exit': fresh.returncode,
                          'fresh_cuda_output': fresh.stdout + fresh.stderr}), flush=True)
        assert (fresh.returncode == 0) == explicit
        output, _ = attached.communicate('continue\n', timeout=60)
        print(output, flush=True)
        passed = 'PASS-AFTER-UPDATE' in output and attached.returncode == 0
        if explicit:
            assert passed, (attached.returncode, output)
        print(json.dumps({'explicit_devices': explicit, 'pid': before,
                          'existing_cuda_context_passed': passed,
                          'expected_result_observed': True}), flush=True)
    finally:
        docker('update', '--restart=no', cid, check=False)
        docker('stop', '--time', '2', cid, check=False)
        if attached is not None and attached.poll() is None:
            attached.communicate(timeout=10)
        docker('rm', cid)
