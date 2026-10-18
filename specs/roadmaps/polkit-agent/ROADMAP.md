# ROADMAP: sy PolicyKit agent

Source: `specs/journeys/JOURNEY-20261018-1635.md`.

## Step 1 — Model the authentication conversation

Files: new polkit module in `crates/sy-desktop-shell/src/`.
Tests: actual PAM conversation messages map to phone/FIDO guidance, masked
password input, completion, and cancellation without exposing secret values.

- [x] Conversation tests pass.
- [x] Credentials travel only to the privileged PolicyKit helper.

## Step 2 — Register the agent and show the dialog

Files: polkit module, desktop shell CLI, launcher routing.
Tests: registration with a temporary D-Bus authority, request isolation and
cancellation; live pkexec authorizes through the real helper.

- [x] Agent verifies the authority sender and uses its offered identities.
- [x] Dialog shows action, identity, factor guidance, PAM messages, and cancellation.
- [x] Producers and consumers compile behind the GUI feature.

## Step 3 — Deploy and verify

Files: Niri config, package metadata, authentication docs.
Tests: managed startup configuration and CLI help; live phone unlock.

- [x] Managed deployment replaces lxpolkit startup after successful validation.
- [x] make lint, make test, make lint-spark, make test-spark pass.
- [x] Journey acceptance criteria satisfied.
