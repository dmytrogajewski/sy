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

.PHONY: build release test test-npu lint fmt fmt-check audit bench install help

build:
	cargo build

release:
	cargo build --release

test:
	cargo test --all-targets

test-npu:
	cargo test --all-targets --features test-npu

lint:
	cargo clippy --all-targets -- -D warnings

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

help:
	@echo "Targets:"
	@echo "  build       — debug build"
	@echo "  release     — release build"
	@echo "  test        — unit + integration tests (no NPU)"
	@echo "  test-npu    — also run NPU-backed tests (daemon must be stopped)"
	@echo "  lint        — cargo clippy --all-targets -- -D warnings"
	@echo "  fmt         — cargo fmt --all"
	@echo "  fmt-check   — cargo fmt --all -- --check"
	@echo "  audit       — cargo deny check (skipped if not installed)"
	@echo "  bench       — cargo bench --all-targets"
	@echo "  install     — release + cp to ~/.local/bin + restorecon"
