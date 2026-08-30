# Support

<!-- Rendered from the `/documenter support` template (GitHub default
     community health files:
     https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/creating-a-default-community-health-file).
     Voice anchored on README.md, CONTRIBUTING.md, and SECURITY.md. -->

`sy` is an Agentic OS layer for Fedora maintained by volunteers. This
page tells you where to land each kind of request so the right pair of
eyes sees it and nothing falls into the maintainer's inbox.

## Where to ask

| You have a... | Go to... |
|---|---|
| Usage question, design discussion, "is this the right tool for X?" | GitHub Discussions (if enabled on the repo); otherwise a GitHub issue prefixed `question:` |
| Reproducible bug | GitHub issue using the **bug report** template |
| Feature idea | GitHub issue using the **feature request** template |
| Suspected security vulnerability | **Do not** open a public issue. See [`SECURITY.md`](SECURITY.md) for the private email channel. |
| Code of Conduct concern | The enforcement contact in [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md). |

This mirrors the hedge in [`CONTRIBUTING.md`](CONTRIBUTING.md): the
repo may or may not have GitHub Discussions enabled at any given time.
If the **Discussions** tab is visible on the repository, prefer it for
anything that is not a bug or a vulnerability. If it is not, open a
regular issue and prefix the title with `question:` so the maintainer
can triage it separately from defect reports.

Please do not email the maintainer for usage questions. The private
email channel is reserved for vulnerability reports.

## Before you ask

Save yourself (and the maintainer) a round trip:

1. **Read the start-here page.** [`docs/intro.md`](docs/intro.md)
   maps tutorials, how-tos, reference, and explanation.
   [`README.md`](README.md) describes the planes
   (`aiplane`, `agt`, `knowledge`, `power`, `file`, `spark`, `stack`,
   `syauth`) and `sy apply`.
2. **Search existing issues and discussions.** Someone may have hit
   the same wall already.
3. **Run `sy doctor`.** Many setup problems surface as a failed
   `doctor` check with an actionable hint; paste the output in your
   question.
4. **Capture your environment.** `sy --version`,
   `cat /etc/fedora-release`, and the relevant `journalctl --user -u
   sy.target -b` snippet shrink the maintainer's reproduction loop.

## What to include in a question

A good question carries:

- The **plane** you are interacting with (`aiplane`, `knowledge`,
  `power`, `stack`, `syauth`, or the `sy apply` layer).
- The **command** you ran and the **output** you got, both copied
  verbatim (use fenced code blocks).
- Your **version and host** (`sy --version`, `cat /etc/fedora-release`).
- **What you expected** to happen and **what you saw** instead.
- Any **`configs/` or unit-file overrides** that diverge from
  upstream `main`.

## Response expectations

`sy` is maintained by volunteers on a best-effort basis. The
maintainer aims to:

- Acknowledge new questions and bug reports as soon as practical,
  with no strict SLA.
- Triage and respond to vulnerability reports under the timelines
  promised in [`SECURITY.md`](SECURITY.md) (acknowledgement within 7
  days, fix or mitigation within 30 days of acknowledgement).

If a thread has been quiet for a couple of weeks and you are still
blocked, a polite bump on the same thread is welcome. Please do not
open a duplicate.

## Things this project does not provide

- **Commercial support contracts or SLAs.** There is no paid tier.
- **Per-user installation help on non-Fedora distributions.** `sy` is
  scoped to Fedora 43; reports against other distros are accepted but
  fixes are not promised.
- **Hardware procurement advice.** The NPU plane targets AMD Ryzen AI
  (XDNA2) silicon; if you do not have it, the `Cpu` execution provider
  works for tests but is not the supported deployment.

## Contributing back

If your question turns into a code change or a docs improvement, see
[`CONTRIBUTING.md`](CONTRIBUTING.md) for how to open a PR. Even a
small fix to a confusing sentence in `README.md` is welcome.
