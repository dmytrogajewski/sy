# CLAUDE.md

> **See also: [`AGENTS.md`](AGENTS.md)** — the shared coding-agent
> persona, non-negotiables, working loop, and NPU-plane norms. This
> file (`CLAUDE.md`) covers the rice-level "no snowflakes" rule and the
> CLIG + agent-friendly CLI conventions; `AGENTS.md` covers everything
> else (tests, dead code, NPU specifics, file layout). The `/<skill>`
> commands under `.claude/commands/` are skill-specific playbooks
> (e.g. `/bug`, `/implement`, `/npu-prep`, `/workload`, `/debug-npu`).

## Core rule: no snowflakes

Every environment change the user requests must be productivized inside this repo (`configs/` or the `sy` app itself). All state must be managed automatically and reproducibly.

- If the user asks to install a package, change a dotfile, tweak a service, set an env var, or modify any part of their environment — encode it in `configs/` or in `sy` so it is applied declaratively.
- Never make a one-off manual change on the host. No ad-hoc `~/.bashrc` edits, no manual `systemctl enable`, no hand-edited config files outside the repo.
- If something currently lives outside the repo and the user asks to change it, first bring it under `sy`/`configs/` management, then change it there.
- `sy` should be the single source of truth. Running `sy` on a fresh machine must reproduce the exact environment.
- If a requested change cannot be expressed declaratively yet, extend `sy` (or add the appropriate module to `configs/`) so it can — do not fall back to manual steps.

Snowflake approach is prohibited.

## CLI design: CLIG + agent-friendly

`sy` must follow the [Command Line Interface Guidelines](https://clig.dev/) and be usable by agents as a first-class consumer, not just humans.

CLIG essentials:
- Human-first help: `sy --help` and `sy <cmd> --help` are complete, concise, and show examples.
- Conventional flags: `-h/--help`, `-v/--verbose`, `-q/--quiet`, `--version`, `--no-color`; respect `NO_COLOR` and `TERM=dumb`.
- Output goes to the right stream: primary output on stdout, logs/diagnostics on stderr.
- Meaningful, non-zero exit codes on failure; zero only on success.
- Errors are actionable: what failed, why, and what to try next.
- Idempotent by default; destructive actions require confirmation or an explicit `--yes`/`--force`.
- Prefer subcommands (`sy apply`, `sy diff`, `sy status`) over flag soup.
- Config precedence: flags > env vars > config file > defaults. Document it.

Agent-friendly requirements:
- `--json` (or `--output json`) on every command that produces output; stable, documented schema.
- Non-interactive by default when stdin is not a TTY; never prompt unless a TTY is attached. Provide `--yes` to bypass prompts explicitly.
- Deterministic, parseable output: no spinners/animations when not a TTY, no ANSI escapes when `NO_COLOR` is set or stdout is piped.
- Dry-run everywhere state changes: `sy <cmd> --dry-run` prints the planned diff without applying.
- `sy diff` shows pending changes; `sy apply` applies them; both machine-readable with `--json`.
- Stable, documented exit codes (e.g. 0 ok, 1 generic error, 2 usage error, 3 drift detected).
- Structured logs on stderr with `--log-format json` for agent consumption.
- Every flag also settable via env var (`SY_*`) so agents can configure without rewriting argv.
- No hidden global state: same inputs + same repo state = same result.
