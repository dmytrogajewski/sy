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

### Added

- User-facing documentation site (Docusaurus) under `website/`,
  fed by the Diátaxis tree in `docs/`: start-here page, search and
  agent tutorials, NPU / MCP / Spark / theme / doctor / power
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

### Changed

- The `knowledge` plane now consumes the `aiplane` daemon through thin
  facades: every embedding request crosses the JSON-over-Unix-socket
  IPC, so the "one process per NPU" rule holds even when several
  consumers are active.
- The `stack` bar aligns under `waybar`, picks glyphs by item type, and
  shows hover previews so the bar reads at a glance without expanding.
- Internal layout follows SPEC §4.4: the agent sandbox, the `aiplane`
  scheduler, and the observability core move into dedicated modules.
  Public CLI surface is unchanged.

### Fixed

- The memory plane now starts at login because `sy apply` enables
  `sy.target`, so a fresh `sy apply` on a clean account no longer
  leaves the user-level supervisor disabled.

[Unreleased]: https://github.com/dmytrogajewski/sy/compare/v0.1.0...HEAD
