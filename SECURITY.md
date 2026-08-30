# Security policy

<!-- Rendered from the `/documenter security` template (Standard SECURITY.md per
     GitHub's security-policy guidance:
     https://docs.github.com/en/code-security/getting-started/adding-a-security-policy-to-your-repository).
     Voice anchored on README.md and AGENTS.md. -->

`sy` is an Agentic OS layer for Fedora — a single Rust binary that owns
real privilege on the host. The current surface includes:

- A user daemon that holds `CAP_*` ambient grants from its systemd unit
  (see [`AGENTS.md`](AGENTS.md), "Cap grants live in the systemd unit").
- The `syauth` PAM module (`pam_syauth.so`) wired into `/etc/pam.d/sudo`
  by `sy syauth install-pam` (see [README §`syauth`](README.md)).
- SELinux policy under [`configs/selinux/`](configs/selinux/).
- Sole owner of `/dev/accel/accel0` from the `aiplane` daemon
  ("one process per NPU" — see [`AGENTS.md`](AGENTS.md)).
- The Spark HTTPS agent and root Docker executor on a DGX Spark
  (`sy spark`, `sy-spark-agent.service`, `sy-spark-executor.service`).

Treat vulnerabilities in any of the above as in scope.

## Supported versions

The project is pre-`1.0.0`. Only the latest tagged release and the
current `main` branch receive security fixes.

| Version  | Supported          |
|----------|--------------------|
| `main`   | yes                |
| `0.1.0`  | yes                |
| older    | no                 |

When a `0.2.0` cut lands, `0.1.0` moves to "no" and this table is
updated in the same change.

## Reporting a vulnerability

Please **do not** open a public GitHub issue, pull request, or
discussion for a suspected vulnerability.

Email **<dmytrogajewski@gmail.com>** with:

- A description of the vulnerability and the affected plane
  (`aiplane`, `agt`, `knowledge`, `power`, `stack`, `syauth`, or the
  `sy apply` declarative layer).
- Reproduction steps or a proof-of-concept (a minimal `sy` invocation,
  a config snippet under `configs/`, or a unit-file diff is ideal).
- The affected version (`sy --version`) and the host Fedora release
  (`cat /etc/fedora-release`).
- Whether the issue requires physical access, a logged-in session, or
  is reachable from an unauthenticated network position.

If you need encryption, request a PGP key in your first message and
one will be returned out-of-band.

## Response targets

- **Acknowledgement: within 7 days** of receipt. The reply confirms
  receipt, assigns a tracking handle, and asks any clarifying
  questions.
- **Fix or mitigation: within 30 days** of acknowledgement for
  confirmed issues. A "mitigation" can be a documented workaround, a
  configuration change shipped under `configs/`, or a patch — whichever
  closes the exposure first.

If a fix needs more than 30 days (for example, a coordinated kernel
or AMD-driver change), the maintainer will keep you informed in
writing and agree a disclosure date with you.

## Disclosure and credit

- The maintainer prefers coordinated disclosure: the fix lands first,
  the advisory is published once users can upgrade.
- Reporters are credited in the release notes and in the
  [`CHANGELOG.md`](CHANGELOG.md) `### Security` entry unless you ask
  not to be named.
- No bug-bounty program. This project is maintained by volunteers;
  the only currency is credit.

## Out of scope

The following are not vulnerabilities in `sy` itself and should be
reported upstream:

- Bugs in ONNX Runtime, AMD VitisAI EP, AMD Quark, the XDNA driver,
  or the Ryzen AI venv — report to AMD or the upstream project.
- Bugs in `qdrant`, `niri`, `waybar`, `yazi`, BlueZ, or any other
  third-party component declared as a dependency in
  [`Cargo.toml`](Cargo.toml) or installed via `configs/`.
- Snowflake misconfigurations performed by hand outside the `sy apply`
  declarative layer (see [`CLAUDE.md`](CLAUDE.md), "no snowflakes").
  If a hand-edited file on the host is exploitable but the
  repo-managed equivalent is safe, that is a host-state issue, not a
  `sy` vulnerability — though a hardening suggestion is still welcome.
