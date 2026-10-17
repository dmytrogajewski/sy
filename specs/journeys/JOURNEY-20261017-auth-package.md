# Optional authentication package

## User journey

A Fedora user installs `sy` and `sy-system-auth` without `sy-desktop`.
`sy syauth --help` and `sy syauth doctor` use the standalone authentication
executable. The Waybar pill and PAM setup commands remain available when the
desktop package is installed. The proprietary or external syauth daemon and
PAM module are separate host prerequisites.

## Acceptance

- The base launcher names the auth RPM when it is absent and delegates to its
  executable when present.
- The standalone CLI accepts the root and target options forwarded by the
  launcher.
- The auth RPM owns the executable, SELinux policy sources, and installer
  script; it does not require the desktop RPM.
- The packaged desktop omits the auth Waybar pill when that optional RPM is
  absent and restores it after installation and `sy apply`.
- A base-only reinstall leaves auth unselected; `--all` selects it.
- The RPM package passes its built-binary smoke test.

## Verification

Run `cargo test -p sy-auth --all-targets`,
`cargo test -p sy-launcher --all-targets`, `cargo test --test rpm_reinstall`,
and `make rpm-native` on Fedora. Check `sy syauth doctor` on a host with the
external syauth runtime installed.
