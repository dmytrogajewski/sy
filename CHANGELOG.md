# Changelog

<!-- Rendered from the `/documenter changelog` template
     (Keep a Changelog 1.1.0 shape:
     https://keepachangelog.com/en/1.1.0/).
     Voice anchored on README.md, AGENTS.md, CONTRIBUTING.md. -->

All notable changes to `sy` are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).
Commit subjects follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/);
see [`CONTRIBUTING.md`](CONTRIBUTING.md) for the SemVer policy and the
commit-message convention that feeds this file.

The `[Unreleased]` section below is a lossy seed of the most recent
visible work, not a full history. Earlier changes are not reconstructed
here.

## [Unreleased]

### Fixed

- **Silence stopped being indexed as content.** 150 of the 4 132
  `telegram-voice` points were not speech: whisper answers a silent round
  video with `""`, one of its own labels (`[Music]`, `(footsteps)`), or a
  syllable hallucinated from noise (`-`, `(`, `в`, a lone U+FFFD), and
  `voice_records` turned every one of them into a point. Voice transcripts are
  67 % of the search-visible corpus, so these degenerate one-character vectors
  were routinely ranked *above* real messages — two of them topped the query
  that found the bug. `transcribe::has_speech` now gates the transcription
  route at 4 signal characters, measured below every real word (129 sidecars
  carry zero, 26 carry 1-3, the median is 84). Two traps the corpus sprang
  on the way: a flat character count is script-biased, so the signal is
  morpheme-weighted and `你好吗` ("how are you?", 3 chars) survives alongside
  `спасибо` (7); and braces are not annotations, so a Rust chunk of
  `{{ kind: … }}` is not scored as silence — the first scan called 382 real
  `agent-history` chunks empty before that was fixed. Rejected transcripts stay
  cached, so a silent file is not re-transcribed every pass, and
  `sy knowledge prune [--apply]` re-applies the gate to points indexed before
  it existed — a re-index cannot heal them, because the unchanged export JSON
  makes the pass skip the file by content hash. See
  `specs/bugs/BUG-20260927-2355.md`.

### Fixed

- **Bulk embedding no longer jumps the queue it was supposed to stand in.**
  `aiplane.batch` was dispatched straight to the workload backend and the
  knowledge plane's own index pass called `Supervisor::run_batch` in-process,
  so neither saw the strict-priority scheduler: no per-class admission, no
  queue caps, no `Realtime > Interactive > Background > Batch` ordering, no
  `Overloaded` backpressure, no entry in `Status.queue_depths`, and no
  inflight record — so `sy aiplane cancel <request_id>` could not even resolve
  a batch's workload. The cause was structural: `scheduler::Request` carried
  one input, so a batch had no representation admission could accept.
  `Request` now carries `Vec<WorkloadInput> -> Vec<WorkloadOutput>` (a `run`
  is a batch of one), `AiplaneDispatch::run` is deleted so there is no
  unadmitted door, `admit_blocking_batch` replaces the daemon's direct
  supervisor call, and bulk embedding admits at `Background` instead of
  `Interactive`. Being admitted also makes a pass *preemptable*: a group that
  loses to a foreground search halves and retries, and at the one-chunk floor
  it sits below the 200 ms escape threshold where preemption cannot reach it
  — the progress guarantee. That retry has not fired once in practice,
  though, and the reason is honest to say out loud: an ONNX call cannot be
  aborted mid-flight, so the watchdog reclaims a group only at its boundary.
  The bound that actually governs a waiting search is the group size, so
  `EMBED_IPC_MAX_CALL` is now *derived* from the escape budget (250 ms /
  31 ms per passage = 8, was 64) rather than asserted. Measured on the live
  machine: a foreground search behind a 512-chunk pass went from 1.96 s to
  0.28 s at unchanged throughput (31.6-32.1 chunks/s at every group size from
  4 to 64, so the shrink costs nothing). A regression test drives the pre-fix
  path and fails on it: three admitted batches reached the backend with the
  dispatcher parked, the `Interactive` one last. See
  `specs/bugs/BUG-20260927-2010.md`.

- **`sy knowledge search` stopped answering questions it had already
  answered.** The rerank ONNX export bakes `sigmoid(logits[..])` into the
  graph, so its scores are probabilities in `[0,1]`;
  `knowledge::calibrate::confidence()` assumed *raw* logits and applied a
  second sigmoid, squashing every confidence into `[0.25, 0.54]` so the
  documented `0.5` abstain cutoff sat inside the range of correct answers.
  Measured on the live corpus: 5 of 15 answerable golden queries came back
  `{results: [], abstained: true}` while the gold chunk sat at rank 1 with a
  0.85–1.00 relevance score. Confidence is now the top-1 probability itself,
  discounted up to half by a tied rival; correct answers score 0.76–0.98 and
  irrelevant noise 0.0001–0.005, so the cutoff sits in an empty margin. The
  eval harness was compounding it: it requested abstention and then scored
  `recall@k` on the (empty) response, so `recall@1 == recall@5` across the
  whole suite and a *policy* failure was reported as a *retrieval* one.
  `eval` now measures recall on the un-abstained ranking and gates
  `false_abstain_rate` as a ceiling. Four `date-range` golden rows were also
  rebuilt — they windowed 2025–2027, a period with **zero** indexed points,
  because they had been derived from the Telegram export's file mtime instead
  of the per-message `date` the payload already carries. On the live index:
  recall@1 0.400 → **0.867**, recall@5 0.400 → **0.933**, MRR 0.400 →
  **0.900**, false-abstain rate 0.333 → **0.067**. See
  `specs/bugs/BUG-20260927-1910.md` (which also retracts that doc's own
  bogus "no per-message dates" finding in BUG-20260927-1610).

- **A wiped model cache no longer takes the knowledge plane down in
  silence.** Deleting `~/.cache/sy/**/<stem>.bf16.onnx` — which `sy disk`
  itself does, see the next entry — made `sy knowledge daemon` exit before its first
  status write; systemd latched `sy-knowledge.service` `failed` after
  five fast retries, and the `custom/sy-knowledge` bar applet — which
  rendered *nothing* for a dead daemon — looked identical to an
  unconfigured rice for four days. Now: the applet renders a visible
  `🧠 !` (`class:"down"`, `hidden` reserved for "never ran here");
  the daemon starts **degraded**, keeps serving IPC + the existing
  index, suspends indexing while a critical worker is missing, and
  heals itself when the artifact returns; `RestartSec`/`StartLimit*`
  are retuned (30 s × 10 within 15 min) and spelled where systemd
  actually reads them; `sy doctor` gained `aiplane.model_artifacts`
  (Fail + the exact `prep_npu_workload.py` command, dangling-symlink
  aware), reports `Warn` instead of `Skip` for an enabled-but-socketless
  unit, and stops inventing `sy-aiplane.service` / `sy-agt.service` in
  its fix hints. See `specs/bugs/BUG-20260927-0119.md`.
- `prep_npu_workload.py` now compiles the VAIP partition under the
  cache key the Rust worker actually requests (`_o1` for `embed`,
  `_b<batch>` otherwise) instead of its own `_b1` convention, so
  prep-time compile is no longer orphaned; `--cache-key` overrides.
  `rerank` defaults to the FP32→BF16 initializer shrink, without which
  VAIP aborts on the 2 GiB `ModelProto` cap.
- `EmbedWorkload` can reach its CPU fallback again: the loader bailed on
  a missing BF16 export before consulting the FP32 sibling that was
  still on disk.
- The `set-up-npu` how-to's prep command had no `--workload` (so it
  could not run) and pointed at the retired `~/.cache/sy/npu-embed/`
  layout.
- **An orderly worker shutdown no longer ends in SIGSEGV.** ONNX Runtime's
  VitisAI EP registers a library finalizer that touches state ORT has already
  destroyed, so every `sy aiplane worker` stop died under `_dl_call_fini`:
  ~330 MB of core files per restart, `status=11/SEGV` in journald for a
  shutdown that was actually clean, and a `sy doctor` warn that could never
  clear while the plane was in use. `std::process::exit` walks into the same
  handlers, so the worker now leaves through `_exit` *after* flushing logs,
  unloading the workload (handing `/dev/accel` back early), and unlinking its
  socket. Verified: zero new cores and zero `SEGV` lines across post-fix
  restarts. See `specs/bugs/BUG-20260927-0400.md`.
- **`knowledge_get_chunk` can now find the chunks `knowledge_search`
  returns.** The daemon *re-derived* each hit's `chunk_id` as
  `point_id(payload.file_path, payload.chunk_index)` instead of returning the
  point id qdrant gave back — but pipelines hash their own domain key
  (telegram hashes the anchor *message id*, while `chunk_index` is the
  *window ordinal*), so the advertised id named a point that does not exist.
  Every REQ-10 drill-down answered `{"chunk": null}` across the whole 115k
  point corpus, indistinguishable from a legitimate "unknown id" miss. Hits
  now carry qdrant's own id verbatim, and `sy knowledge search --json`
  publishes `chunk_id` so the round trip is testable outside MCP. See
  `specs/bugs/BUG-20260927-0316.md`.
- **`sy disk` cleanup no longer eats the NPU plane.** The `user-cache`
  strategy swept `find ~/.cache -type f -mtime +30 -delete`, and model
  weights are written once by prep and never re-touched, so every NPU
  install quietly becomes multi-GB of "stale cache" at 30 days old — that
  is what wiped this machine's plane. The subtree named in `PROTECTED`
  (`~/.cache/sy`) is now skipped by one shared `find_args()` used by both
  the probe and the delete, so the fuzzel menu can neither destroy nor
  advertise provisioning as reclaimable; ordinary caches are unaffected.
- **`sy aiplane run --workload stt|ocr|vad` works against a running
  daemon.** `Supervisor::run_batch` connected straight to the deterministic
  worker socket without calling `ensure()`, so any kind outside the startup
  warm set (`embed`, `rerank`) answered `sy-aiplane worker not reachable` and
  forked nothing — no log line to follow. The CLI's documented daemon-down
  fall-through ran the workload in-process instead, which is the only shape
  `tests/workload_stt.rs` had ever exercised, so CI stayed green throughout.
  `run_batch` now raises the kind first (900 s budget) and then connects.
  Verified end to end: worker spawned and `ready` at +7 s, transcript returned
  over IPC. See `specs/bugs/BUG-20260927-0420.md`.
- **Whisper no longer freezes the knowledge plane.** With the `.rai` cache
  swept, the first STT load ran the *entire* AIE compile inside
  `sy-knowledge.service`, whose `MemoryHigh=12G` also governs every worker the
  daemon spawns as a child. The compile peaks at 19.5 GiB, so the kernel
  charged the cgroup and parked its threads in `__mem_cgroup_handle_over_high`:
  347 871 throttle events, 70 % `memory.pressure full`, ~0 % CPU, a worker
  stuck in `loading` past the 900 s raise deadline, and a `stop` that had to
  wait for the watchdog because `D`-state threads cannot take SIGKILL — all
  while the host sat with 35 GiB free. The compile now belongs to
  `prep_npu_workload.py --workload stt` (7 s when already warm, idempotent: it
  reuses artefacts instead of re-downloading 3.1 GB), the worker refuses a cold
  compile in 0 s with the prep command, `--json` stays parseable by moving the
  AIE compiler's `printf` traffic off fd 1, and `MemoryHigh` is 16 GiB against
  the measured 10.1 GiB warm peak. See `specs/bugs/BUG-20260927-0910.md`.
- **Chunks can no longer outgrow the embedder's window.** `chunk_sized`
  budgeted *whitespace tokens* (640) while `multilingual-e5-base` is exported at
  a static `(1, 512)` shape and `encode()` does `ids.truncate(512)` — so ~30 % of
  every generic chunk, and for whitespace-poor payloads nearly all of it, was
  written to qdrant without ever being embedded: indexed green, unsearchable
  forever. Minified JSON and base64 make a whole file one whitespace token, and
  this corpus grew a single chunk of 1 039 923 characters. There is now a hard
  `MAX_CHUNK_CHARS = 1800` (~512 wordpieces) alongside the token target, with
  oversized tokens cut on char boundaries so the cap is absolute; overlap and
  dense `chunk_index` numbering are preserved, so short documents are unchanged.
  Measured first: 94.9 % of indexed chunks exceed the cap, but only 1.3 % of the
  search-visible ones (the rest is `agent-history`, which search excludes) —
  which is why this ships without a multi-hour full re-embed. See
  `specs/bugs/BUG-20260927-1215.md`.
- **Daemon log sinks that never existed are back.** `main` installed the CLI
  `tracing` subscriber before clap dispatch, so each daemon subcommand's later
  `obs::init(Mode::Daemon { .. })` hit `tracing`'s one-subscriber-per-process
  rule and degraded to a documented no-op: no `tracing_journald` layer and no
  dual-sink rolling JSONL in any running plane. SPEC §4.6's mitigation for the
  journald silent-drop bug was dead code, and `$XDG_STATE_HOME/sy/logs/` was
  375 zero-byte daily files. `main` now asks the parsed command first
  (`Cmd::installs_own_subscriber`, `src/supervision/log_scope.rs`) and hands the
  subscriber slot over; a plane that forgets to register says so on stderr at
  startup instead of losing its sinks quietly, and the daily roller finally got
  a retention sweep (7 days). See `specs/bugs/BUG-20260927-1500.md`.
- **`make docs-lint` is green again.** The Codex configuration citation pointed
  at a `learn.chatgpt.com` path that does not exist; it now names the canonical
  `developers.openai.com/codex/config-reference` that
  `specs/research/spark-model-serving/SPEC.md` already cites, and both
  OpenAI-docs hosts are excluded from lychee because their CDN answers `403` to
  every non-browser client (verified with `curl -A 'Mozilla/5.0'`).
- **`make eval` no longer claims to gate CI.** The golden-set floors and the
  `Makefile` said GitHub Actions runs them; it does not — scoring needs the live
  `sy-knowledge` index, and CI runs `make lint`, `make docs-lint`, and
  `cargo test --doc`. Both now describe it as the on-host pre-push gate it is.
- **The retrieval gate scores the corpus that exists.** The checked-in golden
  set was built from the anecdote in `FEEDBACK.md` (X5 / Магнит / New Year
  2024) rather than the indexed content: `Магнит` occurs 6 times in 25 MB,
  `notes` / `code` / `email` index to zero visible points, and every Telegram
  file — including each voice message — carries the export date as its
  `file_mtime`, so no date window can discriminate it. Nine of fifteen
  answerable rows were unsatisfiable, `make eval` read 0.056 / 0.292, and a
  permanently red gate is a disabled gate. The set is now derived from a
  deterministic stride sample of the live index (24 rows, verbatim gold
  anchors, window-inclusion and window-exclusion date tests) and the floors
  were re-baselined to the measured recall@1 0.400 / abstain 0.583. See
  `specs/bugs/BUG-20260927-1610.md`.

- **`sy knowledge index` can reach the NPU again.** The batch embed route asked
  for an *in-process* supervisor — which only the daemon installs — so
  `sy knowledge index [--source X]` and `sy knowledge bench` died with
  `embed batch: aiplane supervisor not running` whether or not the plane was
  up, while the daemon had advertised a batched `aiplane.batch` method the
  whole time. The CLI now sends the batch to the device owner in 64-passage
  groups (`aiplane::ipc::batch_blocking`, `EMBED_IPC_MAX_CALL`), refuses a
  short answer instead of half-indexing a file, and — when no plane is
  listening — says `systemctl --user start sy-knowledge.service` instead of an
  internal invariant. `--help` no longer promises indexing works without the
  daemon. Measured live: `index --source` indexed 1 file in 261 ms,
  `sy knowledge bench` returned 33.0 chunks/s, and removing the source left
  zero orphan points behind. See `specs/bugs/BUG-20260927-1830.md`.

### Changed

- `make install` relabels `~/.local/bin/sy` unprivileged and only falls back
  to `sudo restorecon` when that fails (loud, with the exact command, instead
  of a silent skip). Under SELinux enforcing an unconfined user may relabel
  their own files, so the mandatory `syauth` phone tap per install is gone.
- Documented the Sparkplane `--allow-network` launch opt-in and its
  `SPARKPLANE_LAUNCH_ALLOW_NETWORK` equivalent across the README, integration
  how-tos, and bridge reference. The opt-in is per launch and leaves filesystem
  sandboxing and approval policy unchanged.

### Added

- **`sy knowledge search --kind telegram-voice`** — transcribed voice notes
  and round videos carry a per-record `kind` that the source registry does
  not own, so `--kind` (typed as the `SourceKind` clap enum) had no name for
  them: 4 132 of the 6 141 search-visible chunks on this host were
  unfilterable from the CLI even though the qdrant filter matched on the
  payload string. `sources::SearchKind` is now the search-facing enum —
  every `SourceKind` plus `TelegramVoice`, delegating its wire strings to
  `SourceKind::as_kebab` so the two cannot drift, with a test that fails if a
  new `SourceKind` variant is left unselectable.
- **`sy knowledge eval` now names the query, not just the metric.**
  `--json` gained a `per_query` array of
  `{query, answerable, rank, confidence, abstained}` (SPEC §4.7's keys stay
  at the root, so the addition is backwards-compatible) and the human
  report prints the same rows. A recall regression can now be told apart
  from an abstain-policy regression from a single `make eval` run —
  conflating the two is what let the double-sigmoid defect hide (see
  above). `Metrics::false_abstain_rate` (share of answerable queries whose
  gold was found but suppressed) is gated by
  `Tolerance::max_false_abstain_rate`.

- **`sy doctor` reads the cgroup, not only the sockets.** New
  `supervision.cgroup_memory_throttle` check walks every installed
  `sy-*.service`, reads its unit scope's `memory.events`, `memory.pressure`,
  and `memory.high`/`peak`/`current`, then WARNs once a plane is being throttled
  (`MemoryHigh` reached, PSI `full avg10` >= 10 %, or peak within 90 % of its
  cap) and FAILs on any OOM kill. The outage of `BUG-20260927-0910` sat at
  `pass=20 fail=0` while the kernel recorded 347 871 throttle events and 69 %
  stall; one `sy doctor` now names it.
- **`configs/systemd/coredump.conf.d/sy.conf`** bounds systemd-coredump:
  upstream defaults permit a 32 GiB core and a spool that may run a volume down
  to 4 GiB free, which is an outage generator beside a 92 %-full root and a
  plane whose cold start peaks at 19.5 GiB. Install with
  `sudo install -D -m 0644 configs/systemd/coredump.conf.d/sy.conf /etc/systemd/coredump.conf.d/sy.conf`.
- User-facing documentation site (Docusaurus) under `website/`,
  fed by the Diátaxis tree in `docs/`: start-here page, search and
  agent tutorials, NPU / MCP / Spark / theme / doctor
  how-tos, Spark and configuration reference, and explanations for
  no-snowflakes, agent-first CLI, and NPU-not-GPU.
- Spark host-install how-to (`sy spark <host> install --dry-run`
  then `--yes` with minisign), plus CLI reference for `sy spark`,
  `sy file`, `sy plugin`, and `sy mon`.
- File-manager tutorial (open the window, hover markdown) split
  from the shell IPC how-to, with troubleshooting on its own page.
- Newcomer language pass across `docs/`, README community files,
  and the docs site: less SPEC/roadmap jargon, NPU-optional
  bring-up verification, `sy plugin` in the CLI reference.
- Product story at the docs entrance: homepage prose and outcome
  cards, start-here page that says what `sy` is before the
  command map, and [What sy is](docs/explanation/what-sy-is.md)
  for the longer why / a-day-with-it / optional-hardware picture.
- Schematic figures in `docs/img/` on the homepage, start-here,
  product story, architecture, and README: stack, apply, planes,
  human-and-agent, NPU ownership, Spark split.
- `sy mon` — on-demand Wayland layer-shell health dashboard backed by
  a 1 Hz `sy-mon-collect.service` aggregator. `Super+m` toggles the
  popup; `sy mon snapshot --json` returns a `SystemSnapshot` over an
  `$XDG_RUNTIME_DIR/sy/mon.sock` IPC socket; `sy mon doctor` folds
  into `sy doctor`; `sy mon mcp` advertises `system.mon.snapshot` and
  `system.mon.history` to MCP-capable agents; `sy mon waybar` emits
  a green/yellow/red waybar custom-module tile that opens the popup
  on click. Wire shape documented in `docs/agents/mon-schema.md`;
  remote-scrape recipe in `docs/admin/mon-remote.md`.
- A `prep_npu_workload.py` helper under `scripts/` exports
  `intfloat/multilingual-e5-base` to ONNX, BF16-quantises it with AMD
  Quark, and runs a one-shot VitisAI compile so the NPU artifact under
  `~/.cache/sy/npu-embed/` is reproducible from a fresh checkout.
- A daemon-in-thread integration-test harness for the `aiplane` plane
  lets `cargo test` exercise the real IPC socket without spawning a
  separate process.
- A sandboxed agent runner (`sy agt`) and an `aiplane` scheduler land
  alongside an observability core that journals every plane decision.
- `sy profile` (visible alias `sy pwr`) and its Waybar tile provide a
  picker, direct selection, and one-click cycling through Fedora's standard
  `power-saver`, `balanced`, and `performance` profiles via `tuned-ppd`.

### Changed

- `sy mon` snapshots now use schema version 2; the removed power panel
  is no longer present in the snapshot document or dashboard grid.
- The `knowledge` plane now consumes the `aiplane` daemon through thin
  facades: every embedding request crosses the JSON-over-Unix-socket
  IPC, so the "one process per NPU" rule holds even when several
  consumers are active.
- The `stack` bar aligns under `waybar`, picks glyphs by item type, and
  shows hover previews so the bar reads at a glance without expanding.
- Internal layout follows SPEC §4.4: the agent sandbox, the `aiplane`
  scheduler, and the observability core move into dedicated modules.
  Public CLI surface is unchanged.

### Removed

- The experimental `sy power` plane, adaptive governor, TuneD replacement
  shim, host policy files, telemetry model, and MCP tool. Fedora's TuneD and
  `tuned-ppd` services are now the sole power-profile managers; `sy profile`
  is only a frontend to their standard D-Bus API.

### Fixed

- The memory plane now starts at login because `sy apply` enables
  `sy.target`, so a fresh `sy apply` on a clean account no longer
  leaves the user-level supervisor disabled.

[Unreleased]: https://github.com/dmytrogajewski/sy/compare/v0.1.0...HEAD
