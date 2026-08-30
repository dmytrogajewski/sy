<!-- Template source: Good Docs Project explanation template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/explanation. Diátaxis quadrant: explanation. -->

# Why the CLI is agent-first

## Why this exists

`sy` is meant to be driven by coding agents as a first-class
consumer, not as an afterthought on a human CLI. An agent cannot
tab-complete its way out of an ambiguous prompt, cannot see a
spinner, and cannot guess an exit code that changes between
releases. The CLI contract is therefore written down and kept
stable: [CLIG](https://clig.dev/) plus a small set of agent rules
in `CLAUDE.md`.

Humans benefit from the same contract. `--help` is complete,
`--json` is always there when a command prints a document, and
`--dry-run` is there when a command would change the machine.

![You type a search; an agent calls the same plane over MCP](../img/sy-surfaces.svg)

## How it works

The rules that show up on every plane:

- Primary output on stdout; logs on stderr.
- `--json` (or `--output json`) on commands that produce a
  document. The schema is documented and does not include a spinner.
- Non-interactive by default when stdin is not a TTY. No prompt
  unless a terminal is attached. `--yes` bypasses prompts when you
  mean it.
- `--dry-run` on state changes. Some commands invert the default
  (`sy auto configure`, `sy knowledge mcp-enable`) and require
  `--apply` so an agent cannot rewrite a file by accident.
- Stable exit codes: `0` success, `1` generic failure, `2` usage,
  `3` warn-only, `4` daemon unreachable / not ready, and
  plane-specific codes above that. Each command documents its codes.
- Every flag also settable via `SY_*` (or the XDG variable the
  flag aliases). Precedence is flag > env > config file > default.
- `NO_COLOR` and `TERM=dumb` strip ANSI.

MCP servers (`sy knowledge mcp`, `sy mon mcp`, `sy file mcp`) speak
line-delimited JSON-RPC on stdio. Prefer the
tool when you already have a session; shelling `sy … --json` is the
fallback, not the native path.

## Trade-offs

- **More flags, less theatre.** There is no progress bar on a
  pipe. Humans on a TTY still get colour unless `NO_COLOR` is set.
- **Dry-run as default on the dangerous commands.** You type
  `--apply` once. An agent that copies a human snippet without
  `--apply` does nothing, which is the safe failure.
## Alternatives we considered

- **A separate `syctl` machine API.** That splits the user model
  and guarantees the two surfaces drift. Rejected: one binary, one
  `--help`.
- **Interactive wizards for apply and MCP.** Fine on a TTY, hostile
  to agents, and they hide the diff. Rejected: `--dry-run` then
  `--apply`.

## See also

- [What sy is](what-sy-is.md)
- [CLI reference](../reference/cli.md)
- [Tutorial: drive sy from an agent](../tutorials/drive-sy-from-an-agent.md)
- [How to wire MCP into your agents](../how-to/wire-mcp-into-agents.md)
