# Spark engine fixture

This fixture freezes the ARM64 child manifest of `hashicorp/http-echo:1.0.0`.
It runs as UID 65532, opens only recipe port 8000, and returns the marker
`sy-spark-fixture-ok` for native and semantic lifecycle checks. It does not
load model weights or request GPU devices.

The signed catalog exposes it only through the explicit recipe
`spark-fixture-http-echo-1.0.0`; default model selection remains vLLM. The
recipe binds the already-verified immutable Ornith repository snapshot
read-only so the ordinary mount and generation invariants remain exercised.

Verify registry metadata without running the image:

```sh
skopeo inspect --override-arch arm64 \
  docker://docker.io/hashicorp/http-echo@sha256:3f5c9a5a28daf63a712bbf45f2fa0741be9cd34339ba598a5c13af02959f108d
```
