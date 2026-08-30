FROM vllm/vllm-openai:qwen38-flash-next@sha256:fc120ece0a388cc0aa1caad4a9f1cd92113484ab7ec2fd0efadd62585be05bf8

ARG RECIPE_REVISION=d2854bfff0a0b6f46984b0941ed1db6010031295
ARG SITE_PACKAGES=/usr/local/lib/python3.12/dist-packages
ARG PLE_LAYER=${SITE_PACKAGES}/vllm/models/qwen3_8_flash_next/nvidia/ple_layer.py

ADD --checksum=sha256:2bca73dd0f77e72937cdfc43312c3fc4d217847d4bb126cf3665bd8caa3108c8 \
    https://raw.githubusercontent.com/blazux/qwen3.8-Flash-DGX/d2854bfff0a0b6f46984b0941ed1db6010031295/src/vllm_ple_mmap.py \
    ${SITE_PACKAGES}/vllm_ple_mmap.py

RUN chmod 0644 "${SITE_PACKAGES}/vllm_ple_mmap.py" \
    && cp "${PLE_LAYER}" "${PLE_LAYER}.orig" \
    && printf '\n\n# Pinned qwen3.8-Flash-DGX PLE mmap hook.\nfrom vllm_ple_mmap import apply as _ple_mmap_apply\n_ple_mmap_apply(Qwen3_8FlashNextNGramEmbedding)\n' >> "${PLE_LAYER}" \
    && python3 -c "import ast; ast.parse(open('${PLE_LAYER}').read())"

LABEL org.opencontainers.image.source="https://github.com/blazux/qwen3.8-Flash-DGX" \
      org.opencontainers.image.revision="${RECIPE_REVISION}" \
      sy.spark.ple-mmap="enabled"
