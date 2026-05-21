<!-- Rendered from the `/documenter pr-template` template
     (rubric R-COMMUNITY-08; GitHub community-profile guidance:
     https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests).
     Voice anchored on CONTRIBUTING.md and AGENTS.md. -->

## Summary

<!-- One paragraph: what changes and why. Reference the plane(s)
     touched (aiplane / agt / knowledge / power / stack / syauth /
     supervision / configs / docs). Link the journey, bug, or
     research SPEC if one drove this change. -->

## Test plan

<!-- Concrete commands you ran and what they proved. Replace the
     placeholder rows with the gates relevant to your change. -->

- [ ] `make lint` is clean (zero clippy warnings; `cargo fmt --all -- --check` passes).
- [ ] `make test` is green.
- [ ] Added or updated tests cover the new behaviour (per AGENTS.md non-negotiables).
- [ ] For NPU-plane changes: ran `make test-npu` against real `/dev/accel/accel0` (daemon stopped first), or explained why it does not apply.
- [ ]

## Docs

<!-- Documentation is a deliverable. Tick every box that applies; if
     a box does not apply, strike it through or write n/a. -->

- [ ] User-facing docs updated in the same change (`README.md`, `docs/tutorials/`, `docs/how-to/`, `docs/reference/`, `docs/explanation/`).
- [ ] `CHANGELOG.md` entry added under `## [Unreleased]` (or n/a — e.g. internal refactor with no user-visible effect).
- [ ] CLI flag, env var, config-file key, or systemd unit grant change is reflected in both `--help` text and `README.md` §CLI cheat-sheet.
- [ ] Rust doc comments (`///`, `//!`) added for new public items; `cargo doc --no-deps` stays clean under `RUSTDOCFLAGS="-D warnings"`.

## Commit policy

- [ ] Every commit is signed off (`git commit -s`) per the [Developer Certificate of Origin 1.1](https://developercertificate.org/) (see [CONTRIBUTING.md §Commit policy](../CONTRIBUTING.md#commit-policy)).
- [ ] Commit subjects follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/); breaking changes carry `!` and a `BREAKING CHANGE:` footer.

## Related

<!-- Link the issue this PR closes. Use additional `Closes #N` /
     `Refs #N` lines for multi-issue PRs. -->

Closes #
