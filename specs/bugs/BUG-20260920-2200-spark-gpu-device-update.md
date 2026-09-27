# Spark loses CUDA access after restart-policy promotion

## Summary

Promoting a healthy engine to Docker's `unless-stopped` restart policy removes
the GPU permissions installed by NVIDIA's legacy runtime hook. This affects
the optimized Qwen engine without any process restart or out-of-memory event.

## Reproduction

On DGX Spark, Docker 29.2.1, systemd cgroups, runc 1.3.4, NVIDIA Container
Toolkit 1.19.0, driver 580.159.03:

1. Start a bounded, non-root diagnostic container using the optimized image
   and `--gpus driver=nvidia,count=all`.
2. `docker exec <exact-container> nvidia-smi -L` succeeds.
3. `docker update --restart unless-stopped <exact-container>` succeeds.
4. Repeat the GPU check: `Failed to initialize NVML: Unknown Error`.
5. Container PID remains unchanged. Driver-unspecified `--gpus all` also fails
   on this host, which has no generated CDI specification.

The failing legacy container was `12d68138923e24831d4139cba7a9d1ae094d072472b0decb174ac8405e52a680`,
PID 2972940. The driver-unspecified container was
`8f185c0839fcb33ee52bb6388a4080ef98d7f06a76fe126f507e66893d92f9b8`.
Both diagnostic containers were removed after disabling their restart policy.

## Expected

Readiness promotion and subsequent lifecycle updates preserve CUDA access.

## Actual

The legacy hook adds device access outside the OCI configuration known to
Docker/runc. A container update reapplies the recorded configuration and
removes those grants. Existing GPU work can hide the loss until a later CUDA
operation requires fresh device access.

## Root Cause

`BollardContainerRuntime::start` requests NVIDIA GPUs through the legacy hook
without recording the compute device nodes in `HostConfig.Devices`.
`promote_restart` then updates the live container immediately after readiness.
This is the lifecycle sequence documented in
[NVIDIA's GPU-access troubleshooting](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/troubleshooting.html#containers-losing-access-to-gpus-with-error-failed-to-initialize-nvml-unknown-error).

The earlier sampler warmup gap was real, but late compilation is not itself
evidence of a sampler defect. A 92-GiB allocation and graph-replay probe passed
without the lifecycle update. The full-model CUDA failures remain subject to
post-fix qualification; the permission-loss trigger is directly reproduced.

## Fix

Record Spark's single compute GPU and CUDA control nodes explicitly in both
managed and qualification container configurations: `/dev/nvidia0`,
`/dev/nvidiactl`, `/dev/nvidia-uvm`, and `/dev/nvidia-uvm-tools`. Docker resolves
the host device numbers. Only read/write access is granted; no wildcard device
rule, new capability, privileged mode, or host runtime reconfiguration is used.
Keep the existing NVIDIA request for driver-library injection.

The optimized image, MTP=3, prefix caching, reduced draft vocabulary, optimized
PLE, CUDA graphs, BF16 KV cache, and 262,144-token context remain enabled.

## Traceability

- Regression: `spark::executor::tests::compute_devices_survive_restart_policy_updates`.
- Implementation: `src/spark/executor.rs`.
- Live diagnostic: explicit-device container
  `8800d286475258aabb0d8e9d066f8334388383a511c560373b4527cd93cd9839`
  retains GPU access after the same update (PID 2976228).
- Repeatable hardware regression and captured results:
  `specs/runs/qwen38-vllm-sampling-20260920/device_update_probe.py` and
  `device-update-probe.log`. An existing CUDA context can still work after the
  legacy update, but NVML and fresh CUDA contexts fail; explicit mappings pass
  all three checks with the same process identity.
- Signed deployment: generation 8, binary
  `sha256:c01dac71cde5a519af3488d1aac55cedac339029a363edce81d16fe70b556f15`.
  Post-promotion NVML, 14 mixed sampled requests, five protocol checks,
  261,038-token retrieval, and two idle Codex smoke tests pass. Expanded
  prefix-cache, concurrent long-prompt, eviction/growth, and cancellation checks
  also pass. Final inspection confirms the original process, zero restarts,
  working GPU access, and the exact signed executable hash.
