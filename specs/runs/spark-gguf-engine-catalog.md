# Spark GGUF engine/catalog real-device run

Date: 2026-08-27. Host: `dgx-spark`. Scope: functional, restart, one streamed protocol turn, and one measured performance run per model; no stress or soak.

## Immutable deployment

- Signed aarch64 `sy`: `sha256:5f4b86aa1e15694269d1803237b78613290de982191846843fa0ea1b01cc8a9e`; signed-install approval `sha256:b3169419223efbc774c4054bba418a2099f36c944181946a41f92c05119ec498`.
- Local x86_64 build from the identical source/config release: `sha256:73ba8d9f5ad23ba9879b678ecfbfa40c185ba292ae8d058ca2a616a3c0cad1ca`, installed byte-identically at `~/.local/bin/sy`; both architectures report `0.1.0`.
- llama.cpp image: `ghcr.io/ggml-org/llama.cpp@sha256:1a9e22a3ab130c186f632fef78c8b0bf8aea5585a6795bf9021ca447c9bf335d`.
- Engine configuration fingerprint: `sha256:bb384adc94255791dabf8631881d7a05c86f6a8f84b42c11814fa6ed7837849a`.
- Protected-platform fingerprint before and after: `7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510` (byte-identical).

Protected versions remained DGX software 7.5.0, Ubuntu 24.04, aarch64, kernel 6.17.0-1022-nvidia, NVIDIA driver 580.159.03, CUDA runtime 13.0, firmware 9A.0B.0F.00.16, Docker 29.2.1, and NVIDIA Container Toolkit 1.19.0. No protected package or reboot command was run.

## Signed external-catalog boundary retry

Independent verification found that the first release compiled the three
operational TOMLs into the installer. The replacement executable reads a
release-directory payload instead. Signed `SHA256SUMS` covers `sy-aarch64` plus
the separate `models.toml`, `llama-cpp.toml`, and `vllm.toml`; activation checks
the signature, exact hashes, approval manifest, schemas, and immutable images
before replacing `/etc/sy/spark`. The coherent payload remains under the active
release so rollback restores matching catalogs. Production source guard
`cargo test --test spark_release_catalog_boundary` exited 0.

- Active Spark release: `releases/0.1.0-8a1eba09cf99c340086cc437800575fce1e5a05ef180d4c0181cdbd5b70d918a`.
- ARM binary: `sha256:af074ac33a45ae64e441c99a713a82bff8383527175bb5da29a49ebad240b969`.
- Signed inventory: `sha256:83602b65a1ce755722991b4e83f3fdbcd594a8d7b74435623790f72e9cda9407`.
- Catalogs: models `e29299b39572bf4a2bb91ce696d2b447d251580a5e5cbb76057e7e7f84ed1b08`, llama.cpp `bb384adc94255791dabf8631881d7a05c86f6a8f84b42c11814fa6ed7837849a`, vLLM `9f65ee4c5b96a35840f0fdcb7eb42418d5ac20241dd01a50a0554f70b596f348`.
- Local x86_64 binary: `sha256:3562861e07916eeb64a1fd9c75dad94dd2fb840792a341f8f624549fde0e8936`; `target/release/sy` and `~/.local/bin/sy` are byte-identical. The local share directory contains the exact signed ARM/config payload above.
- Protected fingerprint immediately before and after replacement: `7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510`.

The signed upgrade, remote binary/config hash probes, service checks, live
`ps --json`, final dry-run fingerprint, and authenticated streamed Responses
smoke exited 0. The preserved Muse generation 7 container was independently
observed unhealthy after service restart, so the empty route registry correctly
returned `spark.route.not-found` (HTTP 404). A functional stop and cached serve
performed no download, produced healthy generation 8 with the same engine and
artifact fingerprints, and the bounded retry completed with final text `OK`
and usage 61 input / 104 output / 165 total tokens.

## Reproducible operator probes

All commands exited 0 unless an explicit diagnostic boundary is stated. Authentication came from mode-0600 credential files; tokens and SSH/sudo input were never captured.

```console
sy spark dgx-spark download <alias> --json
sy spark dgx-spark serve <alias> --name <instance> --json
sy spark dgx-spark ps --json
sy spark dgx-spark logs <instance> --json
sy spark dgx-spark stop <instance> --json
sy spark dgx-spark launch claude --model ornith-1.5:35b -- --bare --tools '' --print 'Reply exactly OK.' --no-session-persistence --output-format stream-json --verbose
sy spark dgx-spark launch opencode --model ornith-1.5:35b -- run --format json 'Reply exactly OK.'
```

The final signed install, protected-platform dry-run, three `download` operations, successful `serve`, `ps`, `logs`, `stop`, restart, Claude launch, OpenCode launch, and all published protocol requests exited 0. Failed-closed diagnostics exited 5 for invalid publication contracts; the regional publisher resolver returned HTTP 403 before the byte-identical public Muse mirror was selected. The signed installer rolled back every rejected activation and left the preceding control plane healthy.

Protocol requests used the pinned CA and a bearer read from its credential file, then POSTed bounded streaming JSON to `/openai/<instance>/v1/responses` and `/anthropic/<instance>/v1/messages`. Text probes required reasoning, text, usage, and terminal events. Tool probes required two independent `lookup` and `patch` calls in one turn. Vision probes supplied the catalog's verified 224x224 opaque-black PNG (`sha256:1b8ab2918e9fb346d8ff5b7579372510b0d8b1566d590dac31aea311fff15fc6`) and required final content `black`.

## Model evidence

| Alias | Immutable model/artifacts | Cold startup | Single-run timing | Memory after the run | Protocol/restart result |
|---|---|---:|---|---|---|
| `qwen3.8:27b` | `unsloth/Qwen3.8-27B-GGUF@4ca720788d1e01f1bff70c033e0d0028fd02e502`; primary `3f227079...b01e`, projector `cbb841a9...e43e`; artifact fingerprint `sha256:672122c8...3fa6a` | 16.671 s measured final cold generation | prompt 407.33 ms/56 tokens; decode 11.75 tok/s (32 tokens); engine-side first-token estimate 492.45 ms | cgroup current 9,507,266,560 B, peak 9,509,982,208 B; GPU process 21,560 MiB | OpenAI text/reasoning/two parallel tools/usage/completed and Anthropic text/reasoning/signature/two tools/usage/finish passed; both image routes returned `black`; generations 3→4 preserved both fingerprints |
| `ornith-1.5:35b` | `ornith-ai/Ornith-1.5-35B-A3B-GGUF@12393612fd4f730ff5aadc23e9b8f9648aa49ceb`; primary `42739874...9d41f`, projector `1921a36a...135d`; artifact fingerprint `sha256:9f8052fd...21d4` | 18.002 s cold; 5.590 s cached restart | prompt 77.90 ms/14 tokens; decode 80.42 tok/s (35 tokens); engine-side first-token estimate 90.33 ms | cgroup current 24,171,687,936 B, peak 24,345,657,344 B; GPU process 22,823 MiB | OpenAI and Anthropic text/reasoning/two parallel tools/usage/finish passed; both image routes returned `black`; generations 1→2 preserved both fingerprints |
| `muse-glimmer:30b` | `lactroiii/Muse-Glimmer-30B-GGUF@c8e212a87fbc137e44463663fb7550ae92079849`; primary `ac7023d6...a6c38`, projector `f48b4523...00c6`; artifact fingerprint `sha256:28b0eaab...fa836` | 20.225 s cold; 19.808 s explicit restart | prompt 334.53 ms/60 tokens; decode 10.65 tok/s (149 tokens); engine-side first-token estimate 427.78 ms | restart cgroup current 1,533,771,776 B, peak 1,537,896,448 B; GPU process 21,073 MiB | OpenAI text/reasoning/two parallel tools/usage/completed and Anthropic text/reasoning/signature/two parallel tools/usage/finish passed; both image routes returned `black`; generations 6→7 preserved both fingerprints |

The engine-side first-token estimate is the logged prompt-evaluation duration plus one logged decode-token duration. It excludes the immediate gateway lifecycle envelope and is reported separately from client-observed Claude TTFT.

Qwen and Ornith restart evidence used engine fingerprint
`sha256:1f7598f1630a75a488cd746b3cf4430c0ce5490f31d6709a728079b8fdeffb3a`;
Muse used the final generic profile fingerprint `sha256:bb384adc94255791dabf8631881d7a05c86f6a8f84b42c11814fa6ed7837849a`.

## Client evidence

- Claude Code 2.1.241 in `--bare`, tool-free, non-persistent headless mode exited 0, streamed `OK`, reported one turn, `end_turn`, client TTFT 410 ms and stream TTFT 210 ms. The returned message carried the immutable Ornith canonical model identity.
- OpenCode headless `run --format json` exited 0, streamed `step_start`, text `OK.`, and `step_finish` with reason `stop` and usage.
- Codex was not invoked with external account credentials. Its exact Responses API path was exercised by the authenticated OpenAI suite, including reasoning, text, parallel functions, usage, and completion.

## Real-device corrections

- The generic llama.cpp context is 65,536 because a real Claude request contained 40,750 input tokens and the former 32,768 profile rejected it.
- Vision policy is declared in the generic engine profile and publication now runs the projector-backed health fixture before exposing image routes.
- The publisher Muse resolver returned HTTP 403 from the Spark with `This model is not available in your region`. The catalog therefore pins public mirror commit `c8e212a87fbc137e44463663fb7550ae92079849`; its selected 19,653,960,832-byte primary and 1,400,328,928-byte projector have the exact publisher SHA-256 values `ac7023d6...a6c38` and `f48b4523...00c6`. No proxy or regional-control bypass was used.
- Muse opens its reasoning channel unconditionally. The generic bounded health adapter therefore requests the lowest template reasoning strength as well as disable-thinking, and its 256-token publication budget remains configuration data rather than model-specific Rust.
- A valid 1x1 opaque-black PNG was classified as `blue` by Muse's perception encoder. The signed generic fixture is now a deterministic 224x224 opaque-black PNG, whose final result was `black` on all three artifacts.
- The pinned llama.cpp server expects string `tool_choice`; its portable `required` form plus a 256-token low-reasoning readiness turn produced a complete streamed tool call. The public Muse suites then produced two parallel calls with 512 output tokens.
- One exploratory strict-schema Muse request caused an upstream CUDA `operation not permitted` abort. Docker restarted the same managed generation automatically, health recovered, and the final non-strict parallel-tool, image, stop, and explicit restart probes all passed; no platform component was changed.

## Artifact and gate ledger

| Model | Verified bytes | Primary SHA-256 | Projector SHA-256 | Download/serve/stop/restart |
|---|---:|---|---|---|
| Qwen3.8-27B | 18,486,785,632 | `3f227079003add2511437e5b1e94812e363385225bf6a9b47b0054a72bc8b01e` | `cbb841a9ee0636b2ec172f5bb8df2ea8dfeb01e90fe7c6126581d662a0b4e43e` | exit 0 / exit 0 / exit 0 / exit 0 |
| Ornith-1.5-35B-A3B | 22,616,285,280 | `42739874cc2ccfdb8523b23fbe52e29b2a7555c8176737ca9ca0b5d59859d41f` | `1921a36a85aee56cd2abd27f46701802c9d85a33474792e600df6c3b282a135d` | exit 0 / exit 0 / exit 0 / exit 0 |
| Muse Glimmer 30B | 21,054,289,760 | `ac7023d6a4c704eb9af54ab53e476a66b7f5b6c0ef2fc4a8dde5253c291a6c38` | `f48b452316f9b213758e8659444029b961a24a07f99a1abb2a9f88b06f7c00c6` | exit 0 / exit 0 / exit 0 / exit 0 |

The pre-mutation raw protected-package inventory is `sha256:1d21121eef75d337179e36823f6554d5f8177d785535e27d195424ed8bca5be4`. The canonical protected-platform document remained `sha256:7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510` at the final dry-run. The deprecated stopped sy-managed state was quarantined with a verified 598,016-byte SQLite backup; no legacy decoder, migration, or catalog merge was introduced.

Final retry gates exited 0: `make lint` completed without warnings and two
consecutive full reruns of `make test` reported 1,863 passing tests with zero
failures. The first attempt exposed one `sy_file` copy-observation timing flake;
its focused rerun and both complete reruns passed.
