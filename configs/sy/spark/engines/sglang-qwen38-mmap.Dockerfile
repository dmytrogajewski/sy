FROM lmsysorg/sglang@sha256:14ed582518584c5c830206b5318a2c2769e68229c3422e48a28b952b3a888bd4

ARG RECIPE_REVISION=04d073518ded5d0db1cddce74d9afb1cdca5eddc

COPY sglang-qwen38-mmap.toml /tmp/sy-image-contract/engine.toml

LABEL org.opencontainers.image.source="https://github.com/hashd1ve/qwen38-flash-next-one-dgx-spark" \
      org.opencontainers.image.revision="${RECIPE_REVISION}" \
      sy.spark.image-contract="v1" \
      sy.spark.base-manifest="sha256:12d3392bdc8be8d35e9a95f191df6aef99c5114bdbefd41bfdc7e760e6d25ec1" \
      sy.spark.persistence-transformer="sha256-2edb46d75172645bc444853563385f480b5bfb872127d0b281fea2f5a8fc6e28.py" \
      sy.spark.page-cache-transformer="sha256-6530076f56575c6754375ba944f0911282999c30bbd2d5b1bdb7e89cf5e5b4aa.py" \
      sy.spark.post-warmup-transformer="sha256-12714616ca51148aeee0089153826230adc15b66031eba3de07586a81f6544db.py" \
      sy.spark.post-warmup-cleanup="flush_transient_allocator" \
      sy.spark.ple-self-test="sha256-708e870359a4d0f390b60096a6428506af013a8aaf7d796783e689f432ff4744.py" \
      sy.spark.persistence-self-test="sha256-bbaec9843bab2d859db87f05cbd8929638aa6caf1e2cc95754edd89ce4dbad3b.py" \
      sy.spark.page-cache-self-test="sha256-e373f3b21a12ce62066d8b1a3bd8390d7946651def00b5d54e976d07b97a8510.py" \
      sy.spark.post-warmup-self-test="sha256-ba23e86521ba43731da3eff8b58bd438529f6a3341de5c351f01ce898b581372.py" \
      sy.spark.architecture-self-test="sha256-f969092584d6a9d2c8e74f6a9720ed7d4bdb51fca69962b88a8470282f88ce78.py" \
      sy.spark.runtime-self-test="sha256-e4492a172636e0cc6d55b8baebf29313d687ca56bb6d5f2a155f4de3f00b78e0.py" \
      sy.spark.runtime-profile="/tmp/sy-image-contract/engine.toml" \
      sy.spark.runtime-network="private-offline" \
      sy.spark.runtime-build-tools="jit-only" \
      sy.spark.runtime-jit-tools="gcc,g++,cc,c++,cmake,ninja,make,nvcc" \
      sy.spark.runtime-rejected-tools="apt,apt-get,dpkg,dpkg-query,git,curl,wget,pip,pip3,uv" \
      sy.spark.runtime-source="/sgl-workspace/sglang" \
      sy.spark.runtime-source-policy="immutable" \
      sy.spark.runtime-scm-metadata="/sgl-workspace/sglang/.git" \
      sy.spark.runtime-user="65534" \
      sy.spark.runtime-writable="/compile-cache,/tmp" \
      sy.spark.cuda-architecture="12.1"

COPY patches/sha256-eeabdde061631c9b606d4ccc7371ff8fb01c6cc034dfe6bad1e4f29a8aa21555.py /tmp/sy-image-patches/sha256-eeabdde061631c9b606d4ccc7371ff8fb01c6cc034dfe6bad1e4f29a8aa21555.py
COPY patches/sha256-708e870359a4d0f390b60096a6428506af013a8aaf7d796783e689f432ff4744.py /tmp/sy-image-self-tests/sha256-708e870359a4d0f390b60096a6428506af013a8aaf7d796783e689f432ff4744.py
COPY patches/sha256-2edb46d75172645bc444853563385f480b5bfb872127d0b281fea2f5a8fc6e28.py /tmp/sy-image-patches/sha256-2edb46d75172645bc444853563385f480b5bfb872127d0b281fea2f5a8fc6e28.py
COPY patches/sha256-bbaec9843bab2d859db87f05cbd8929638aa6caf1e2cc95754edd89ce4dbad3b.py /tmp/sy-image-self-tests/sha256-bbaec9843bab2d859db87f05cbd8929638aa6caf1e2cc95754edd89ce4dbad3b.py
COPY patches/sha256-6530076f56575c6754375ba944f0911282999c30bbd2d5b1bdb7e89cf5e5b4aa.py /tmp/sy-image-patches/sha256-6530076f56575c6754375ba944f0911282999c30bbd2d5b1bdb7e89cf5e5b4aa.py
COPY patches/sha256-e373f3b21a12ce62066d8b1a3bd8390d7946651def00b5d54e976d07b97a8510.py /tmp/sy-image-self-tests/sha256-e373f3b21a12ce62066d8b1a3bd8390d7946651def00b5d54e976d07b97a8510.py
COPY patches/sha256-12714616ca51148aeee0089153826230adc15b66031eba3de07586a81f6544db.py /tmp/sy-image-patches/sha256-12714616ca51148aeee0089153826230adc15b66031eba3de07586a81f6544db.py
COPY patches/sha256-ba23e86521ba43731da3eff8b58bd438529f6a3341de5c351f01ce898b581372.py /tmp/sy-image-self-tests/sha256-ba23e86521ba43731da3eff8b58bd438529f6a3341de5c351f01ce898b581372.py
COPY patches/sha256-f60ccb9f9e350a43155a1a7a20d154be0b7e93c29dacb3db95d397ba910090b2.py /tmp/sy-image-patches/sha256-f60ccb9f9e350a43155a1a7a20d154be0b7e93c29dacb3db95d397ba910090b2.py
COPY patches/sha256-f969092584d6a9d2c8e74f6a9720ed7d4bdb51fca69962b88a8470282f88ce78.py /tmp/sy-image-self-tests/sha256-f969092584d6a9d2c8e74f6a9720ed7d4bdb51fca69962b88a8470282f88ce78.py
COPY patches/sha256-e4492a172636e0cc6d55b8baebf29313d687ca56bb6d5f2a155f4de3f00b78e0.py /tmp/sy-image-self-tests/sha256-e4492a172636e0cc6d55b8baebf29313d687ca56bb6d5f2a155f4de3f00b78e0.py
COPY patches/sha256-9f228eb6db985bd17fb21b051e747841da8fd37ac5e131228c15fa4cca2dc669.py /tmp/sy-image-patches/sha256-9f228eb6db985bd17fb21b051e747841da8fd37ac5e131228c15fa4cca2dc669.py

RUN set -eux; \
    for asset in /tmp/sy-image-patches/sha256-*.py /tmp/sy-image-self-tests/sha256-*.py; do \
        expected="${asset##*/sha256-}"; expected="${expected%.py}"; \
        printf '%s  %s\n' "${expected}" "${asset}" | sha256sum --check -; \
    done; \
    ple_target="$(python3 -c 'import sglang.srt.models.qwen4_exp as module; print(module.__file__)')"; \
    python3 /tmp/sy-image-patches/sha256-eeabdde061631c9b606d4ccc7371ff8fb01c6cc034dfe6bad1e4f29a8aa21555.py "${ple_target}"; \
    python3 /tmp/sy-image-patches/sha256-2edb46d75172645bc444853563385f480b5bfb872127d0b281fea2f5a8fc6e28.py "${ple_target}"; \
    python3 /tmp/sy-image-patches/sha256-6530076f56575c6754375ba944f0911282999c30bbd2d5b1bdb7e89cf5e5b4aa.py "${ple_target}"; \
    python3 -c 'import sglang.srt.models.qwen4_exp as module; assert callable(module._alloc_ple_table)' ; \
    python3 -c 'import ast,sys; source=open(sys.argv[1], encoding="utf-8").read(); ast.parse(source); assert source.count("def _alloc_ple_table(") == 1; assert source.count("_alloc_ple_table(source_weight.shape") == 1; assert source.count("sy.spark.ple-cache/v2") == 1' "${ple_target}"; \
    qsa_target="$(python3 -c 'import sglang.srt.layers.attention.qwen_sparse_attn_backend as module; print(module.__file__)')"; \
    python3 /tmp/sy-image-patches/sha256-f60ccb9f9e350a43155a1a7a20d154be0b7e93c29dacb3db95d397ba910090b2.py "${qsa_target}"; \
    python3 -c 'import ast,sys; source=open(sys.argv[1], encoding="utf-8").read(); ast.parse(source); assert source.count("is_sm100_supported() or is_sm120_supported()") == 1' "${qsa_target}"; \
    tokenizer_target="$(python3 -c 'import sglang.srt.managers.tokenizer_manager as module; print(module.__file__)')"; \
    python3 /tmp/sy-image-patches/sha256-9f228eb6db985bd17fb21b051e747841da8fd37ac5e131228c15fa4cca2dc669.py "${tokenizer_target}"; \
    python3 -c 'import ast,sys; source=open(sys.argv[1], encoding="utf-8").read(); ast.parse(source); assert source.count("obj._dispatched_rids = dispatched_rids.copy()") == 1; assert source.count("force=True") == 1; assert source.count("and not force") == 1' "${tokenizer_target}"; \
    warmup_target="$(python3 -c 'import sglang.srt.entrypoints.warmup as module; print(module.__file__)')"; \
    python3 /tmp/sy-image-patches/sha256-12714616ca51148aeee0089153826230adc15b66031eba3de07586a81f6544db.py "${warmup_target}"; \
    python3 -c 'import ast,sys; source=open(sys.argv[1], encoding="utf-8").read(); ast.parse(source); assert source.count("def flush_transient_allocator(") == 1; assert "drop_caches" not in source' "${warmup_target}"

RUN install -d -o 65534 -g 65534 -m 0700 /compile-cache

ENV HOME=/tmp

USER 65534:65534

RUN set -eux; \
    ple_target="$(python3 -c 'import sglang.srt.models.qwen4_exp as module; print(module.__file__)')"; \
    python3 /tmp/sy-image-self-tests/sha256-708e870359a4d0f390b60096a6428506af013a8aaf7d796783e689f432ff4744.py "${ple_target}"; \
    python3 /tmp/sy-image-self-tests/sha256-bbaec9843bab2d859db87f05cbd8929638aa6caf1e2cc95754edd89ce4dbad3b.py "${ple_target}"; \
    python3 /tmp/sy-image-self-tests/sha256-e373f3b21a12ce62066d8b1a3bd8390d7946651def00b5d54e976d07b97a8510.py "${ple_target}"; \
    python3 /tmp/sy-image-self-tests/sha256-ba23e86521ba43731da3eff8b58bd438529f6a3341de5c351f01ce898b581372.py /tmp/sy-image-patches/sha256-12714616ca51148aeee0089153826230adc15b66031eba3de07586a81f6544db.py; \
    python3 /tmp/sy-image-self-tests/sha256-f969092584d6a9d2c8e74f6a9720ed7d4bdb51fca69962b88a8470282f88ce78.py; \
    python3 /tmp/sy-image-self-tests/sha256-e4492a172636e0cc6d55b8baebf29313d687ca56bb6d5f2a155f4de3f00b78e0.py /tmp/sy-image-contract/engine.toml 12.1

USER root

RUN set -eux; \
    rm -rf /sgl-workspace/sglang/.git /tmp/sy-image-patches /tmp/sy-image-self-tests /tmp/sy-image-contract \
        /usr/local/lib/python3.12/dist-packages/pip /usr/local/lib/python3.12/dist-packages/pip-*.dist-info; \
    chmod -R a-w /sgl-workspace/sglang; \
    rm -f /usr/bin/apt* /usr/bin/dpkg* /usr/bin/git /usr/bin/curl /usr/bin/wget \
        /usr/local/bin/pip* /usr/local/bin/uv

WORKDIR /tmp

USER 65534:65534

RUN set -eux; \
    for tool in gcc g++ cc c++ cmake ninja make nvcc; do command -v "${tool}"; done; \
    for tool in apt apt-get dpkg dpkg-query git curl wget pip pip3 uv; do \
        if command -v "${tool}"; then exit 1; fi; \
    done; \
    test ! -e /sgl-workspace/sglang/.git; \
    test -w /tmp; \
    python3 -c 'import sglang.launch_server; import sglang.srt.models.qwen4_exp'
