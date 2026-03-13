## —— Tests and e2e test ---------------------------------------------------------------------------

TEST_THREADS ?= 1

.PHONY: test
test: core-deps test-deps ## Run all tests
	@echo "🧪 Running tests..."
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --all --exclude e2e_test; \
	else \
		echo "ℹ️ cargo-nextest not found; falling back to 'cargo test'"; \
		cargo test --workspace --exclude e2e_test -- --nocapture --test-threads="$(TEST_THREADS)"; \
	fi
	cargo test --all --doc

.PHONY: e2e-server
e2e-server: ## Run e2e-server tests
	sh $(shell pwd)/scripts/run.sh

.PHONY: probe-e2e
probe-e2e: ## Probe e2e tests
	sh $(shell pwd)/scripts/probe.sh

# E2E tests start a RustFS server each; they must run single-threaded so one test
# does not kill another's server (cleanup kills all rustfs processes).
# The e2e test setup builds rustfs once and only once per run (see common::rustfs_binary_path),
# so you can run with cargo test -p e2e_test or make e2e-test.
# RUSTFS_BUILD_FEATURES=ftps ensures the binary is built with FTPS for protocol tests.
.PHONY: e2e-test
e2e-test: core-deps test-deps ## Run e2e_test crate (single-threaded)
	@echo "🧪 Running e2e tests (single-threaded)..."
	RUSTFS_BUILD_FEATURES=ftps cargo test -p e2e_test -- --nocapture --test-threads=1
