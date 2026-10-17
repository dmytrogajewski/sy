# Desktop configuration with optional feature RPMs

## User journey

A user installs `sy` and `sy-desktop` with only the feature RPMs they want.
They run `sy --root /usr/share/sy apply`. The rendered Waybar rail and niri
keymap contain entries for installed power, agent, authentication, and file
manager features. Installing another feature and applying again adds its
entry. Removing one and applying again removes its entries.

## Acceptance

- Packaged configuration checks feature executables in `/usr/libexec/sy`.
- A checkout retains all feature entries because its monolithic `sy` binary
  still provides those commands.
- Rendered Waybar configuration is valid JSON after comments are removed
  with all optional features present and with none present.
- Packaged GPU and NPU tile actions launch the RPM-provided Foot executable.
- Rendered niri configuration validates; missing features leave no dead
  startup commands or key bindings.
- `sy apply` removes stale sy-owned user-unit links when their source files
  disappear, while preserving unrelated links.
- `sy apply` tracks rendered files and removes a copied file after its source
  disappears only when its bytes still match the last applied version.
- Dry runs report stale files without deleting them. Paths in the tracking
  manifest cannot escape the chosen config target.
- A removed Waybar file triggers a reload; a manifest entry below a symlinked
  directory cannot remove a file outside the config target.

## Verification

Run `cargo test --bin sy packaged_desktop_hides_uninstalled_feature_entries`,
`cargo test --bin sy uninstalled_feature_unit_is_removed_but_foreign_link_is_preserved`,
`make lint`, `make test`, and `niri validate -c` on a rendered niri config.
