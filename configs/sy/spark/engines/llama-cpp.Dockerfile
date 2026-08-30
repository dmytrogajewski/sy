ARG CUDA_VERSION=13.0.1

FROM nvidia/cuda:${CUDA_VERSION}-devel-ubuntu24.04 AS build

ARG SOURCE_REVISION

RUN test -n "${SOURCE_REVISION}" \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        git \
        ninja-build \
    && git clone --filter=blob:none https://github.com/ggml-org/llama.cpp /src/llama.cpp \
    && git -C /src/llama.cpp checkout --detach "${SOURCE_REVISION}" \
    && cmake -S /src/llama.cpp -B /src/llama.cpp/build -G Ninja \
        -DBUILD_SHARED_LIBS=OFF \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_CUDA_ARCHITECTURES=121 \
        -DCMAKE_CUDA_FLAGS=-O2 \
        -DGGML_CUDA=ON \
        -DGGML_CUDA_FORCE_CUBLAS=ON \
        -DGGML_NATIVE=OFF \
        -DLLAMA_CURL=OFF \
    && cmake --build /src/llama.cpp/build --target llama-server --parallel 8

FROM nvidia/cuda:${CUDA_VERSION}-runtime-ubuntu24.04

ARG SOURCE_REVISION

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libgomp1 \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/llama.cpp/build/bin/llama-server /app/llama-server

LABEL org.opencontainers.image.source="https://github.com/ggml-org/llama.cpp" \
      org.opencontainers.image.revision="${SOURCE_REVISION}" \
      org.opencontainers.image.title="llama.cpp" \
      sy.spark.cuda-backend="cublas-sm121-o2"

ENV LLAMA_ARG_HOST=0.0.0.0
WORKDIR /app
ENTRYPOINT ["/app/llama-server"]
