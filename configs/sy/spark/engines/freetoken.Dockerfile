FROM ghcr.io/astral-sh/uv@sha256:8ba8ac26ed7be9ce3f0fbd510f8d26a3fb9b19056efe6c08433baf9762129edd AS uv

FROM nvidia/cuda:13.0.1-devel-ubuntu24.04@sha256:5c36750138dc1447a17dafbb397674f167d3b44ce18d9160d769df114577b35d

ARG FREETOKEN_REVISION=9ef3651309fe4058672f2cc92069238dea06be1b

COPY --from=uv /uv /usr/local/bin/uv
ADD --checksum=sha256:28fe03a63ccc2ce14bf928f77481e0d3f68baccc28d971c3dea8d3ff01d8451f \
    https://github.com/FlashML-org/FreeToken/archive/9ef3651309fe4058672f2cc92069238dea06be1b.tar.gz \
    /tmp/freetoken.tar.gz

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        libomp-dev \
        ninja-build \
        python3 \
        python3-dev \
        python3-venv \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/freetoken/src \
    && tar -xzf /tmp/freetoken.tar.gz --strip-components=1 -C /opt/freetoken/src \
    && rm /tmp/freetoken.tar.gz

ENV CUDA_HOME=/usr/local/cuda \
    PATH=/opt/freetoken/venv/bin:/usr/local/cuda/bin:${PATH} \
    TVM_FFI_CUDA_ARCH_LIST=12.1 \
    UV_LINK_MODE=copy

RUN uv venv --python /usr/bin/python3 /opt/freetoken/venv \
    && uv pip install --python /opt/freetoken/venv/bin/python -e '/opt/freetoken/src[accel]' \
    && /opt/freetoken/venv/bin/ft --version \
    && uv pip freeze --python /opt/freetoken/venv/bin/python > /opt/freetoken/requirements.freeze

LABEL org.opencontainers.image.source="https://github.com/FlashML-org/FreeToken" \
      org.opencontainers.image.revision="${FREETOKEN_REVISION}" \
      sy.spark.cuda-architecture="12.1"
