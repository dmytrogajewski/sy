# sy — common dev targets.
#
# Conventions:
#   - `make test` is fast: in-tree unit + integration tests that don't
#     need a real NPU (FakeWorkload covers daemon plumbing).
#   - `make test-npu` runs the gated `cfg(feature = "test-npu")` tests
#     that hit `/dev/accel/accel0`. The daemon must be stopped (`sudo
#     systemctl stop sy-aiplane.service`) or these will EAGAIN.
#   - `make lint` is the gate before any commit. The Stop hook
#     (.claude/hooks/stop-verify.sh) re-runs the lint subset.

.PHONY: build release test test-npu eval lint fmt fmt-check audit bench install install-system-npu install-system-syauth-selinux docs-lint docs-site help

build:
	cargo build --workspace

release:
	cargo build --workspace --release

test:
	cargo test --workspace --all-targets

test-npu:
	cargo test --workspace --all-targets --features test-npu

# Retrieval-eval golden set (REQ-9). Runs the labelled queries through
# the live index and reports recall@1/5, MRR, abstain accuracy. Exits
# non-zero (drift, code 3) when a metric regresses past tolerance, so CI
# gates on it. Needs the sy-knowledge daemon running with an index.
eval:
	cargo run --quiet -- knowledge eval --json

lint:
	./scripts/check_main_rs_loc.sh 1118
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

audit:
	@if command -v cargo-deny >/dev/null 2>&1; then \
		cargo deny check; \
	else \
		echo "cargo-deny not installed; skipping audit"; \
	fi

bench:
	cargo bench --all-targets

install: release
	cp --remove-destination target/release/sy ~/.local/bin/sy
	@if command -v sudo >/dev/null && [ "$$(getenforce 2>/dev/null)" = "Enforcing" ]; then \
		sudo restorecon -v ~/.local/bin/sy; \
	fi

# Productivized recovery for the initramfs-too-early amdxdna probe
# (see configs/dracut/sy-amdxdna-defer.conf for the failure mode).
# Idempotent — safe to re-run after every kernel update.
install-system-npu:
	./scripts/install-system-npu.sh

# Productivized SELinux policy module so pam_syauth.so can append
# /var/lib/syauth/last.log from xdm_t / sudo_t. Idempotent.
install-system-syauth-selinux:
	./scripts/install-system-syauth-selinux.sh

# Mirror of .github/workflows/docs.yml so contributors can run the
# four-gate docs lint locally before pushing.
#
# Install the linters (one-time per host):
#   npm install -g markdownlint-cli2 cspell
#   # Vale: https://vale.sh/docs/vale-cli/installation/
#   curl -sfL https://github.com/errata-ai/vale/releases/latest/download/vale_3.7.1_Linux_64-bit.tar.gz | tar -xz -C ~/.local/bin vale
#   (cd /tmp && vale sync)  # populates .vale/styles with Microsoft + Google packs
#   # Lychee: https://github.com/lycheeverse/lychee
#   cargo install lychee
#
# `vale` runs in advisory mode (matches the CI workflow). The other
# three are hard gates — first failure exits non-zero.
docs-lint:
	@set -e; \
	if command -v markdownlint-cli2 >/dev/null 2>&1; then \
		echo "==> markdownlint-cli2"; \
		markdownlint-cli2 '**/*.md' '!target/**' '!node_modules/**' '!specs/runs/**' '!.vale/**'; \
	else \
		echo "skip: markdownlint-cli2 not installed (npm i -g markdownlint-cli2)"; \
	fi; \
	if command -v cspell >/dev/null 2>&1; then \
		echo "==> cspell"; \
		cspell --config cspell.json --no-progress --no-summary '**/*.md'; \
	else \
		echo "skip: cspell not installed (npm i -g cspell)"; \
	fi; \
	if command -v lychee >/dev/null 2>&1; then \
		echo "==> lychee"; \
		lychee --config lychee.toml --exclude-path target \
			--exclude-path website/node_modules './**/*.md'; \
	else \
		echo "skip: lychee not installed (cargo install lychee)"; \
	fi; \
	if command -v vale >/dev/null 2>&1; then \
		echo "==> vale (advisory)"; \
		vale --no-exit . || true; \
	else \
		echo "skip: vale not installed (see https://vale.sh/docs/vale-cli/installation/)"; \
	fi

# Production Docusaurus build for the user-facing docs/ tree.
# Requires Node 18+ and website/package-lock.json.
docs-site:
	@set -e; \
	if ! command -v npm >/dev/null 2>&1; then \
		echo "docs-site: npm not found (install Node 18+)"; \
		exit 1; \
	fi; \
	echo "==> docusaurus build (website/)"; \
	npm ci --prefix website; \
	npm run build --prefix website

help:
	@echo "Targets:"
	@echo "  build       — debug build"
	@echo "  release     — release build"
	@echo "  test        — unit + integration tests (no NPU)"
	@echo "  test-npu    — also run NPU-backed tests (daemon must be stopped)"
	@echo "  eval        — retrieval-eval golden set (recall@1/5, MRR,"
	@echo "                abstain accuracy); non-zero on regression (REQ-9)"
	@echo "  lint        — cargo clippy --all-targets -- -D warnings"
	@echo "  fmt         — cargo fmt --all"
	@echo "  fmt-check   — cargo fmt --all -- --check"
	@echo "  audit       — cargo deny check (skipped if not installed)"
	@echo "  bench       — cargo bench --all-targets"
	@echo "  install     — release + cp to ~/.local/bin + restorecon"
	@echo "  install-system-npu — sudo install dracut conf + system units to keep"
	@echo "                       amdxdna out of initramfs (one-time per host;"
	@echo "                       safe to re-run after each kernel update)"
	@echo "  install-system-syauth-selinux — sudo build + load syauth SELinux module"
	@echo "                       so pam_syauth.so can append /var/lib/syauth/last.log"
	@echo "                       from xdm_t (gdm) / sudo_t (one-time per host)"
	@echo "  docs-lint   — markdownlint + cspell + lychee + vale (advisory);"
	@echo "                mirror of .github/workflows/docs.yml"
	@echo "  docs-site   — Docusaurus production build (website/build)"
