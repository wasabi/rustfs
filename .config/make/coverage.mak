## —— Coverage (cargo-llvm-cov) ----------------------------------------------------------------
##
## Prerequisites:
##   cargo install cargo-llvm-cov
##   rustup component add llvm-tools-preview
##
## Doctests are not run under coverage (they require nightly). Use stable for unit/e2e coverage.
## Optional: use COVERAGE_OUTPUT_DIR to change report dir (default: target/coverage)
##
## Use BFD linker for coverage builds when available to avoid LLD duplicate-symbol
## errors with -C instrument-coverage (e.g. arrow_array/arrow_arith in datafusion).
## If ld.bfd is not found, no linker override is set (may hit duplicate symbol on LLD).
## Override: make coverage-unit COVERAGE_RUSTFLAGS="-C link-arg=-fuse-ld=bfd"

COVERAGE_RUSTFLAGS_DEFAULT := $(shell command -v ld.bfd >/dev/null 2>&1 && echo '-C link-arg=-fuse-ld=bfd' || true)
COVERAGE_RUSTFLAGS ?= $(COVERAGE_RUSTFLAGS_DEFAULT)
COVERAGE_OUTPUT_DIR ?= target/coverage
COVERAGE_E2E_DIR    := $(COVERAGE_OUTPUT_DIR)/e2e
## LCOV file for editor integration (Cursor/VS Code Coverage Gutters). Default: repo root.
COVERAGE_LCOV_PATH ?= lcov.info

.PHONY: coverage-check
coverage-check: ## Verify cargo-llvm-cov and llvm-tools are available
	@cargo llvm-cov --version >/dev/null 2>&1 || (echo "Install: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview"; exit 1)

.PHONY: coverage-deps
coverage-deps: ## Install cargo-llvm-cov and llvm-tools-preview if missing
	@command -v cargo >/dev/null 2>&1 || (echo "cargo not found. Ensure Rust is installed and in PATH."; exit 1)
	@cargo llvm-cov --version >/dev/null 2>&1 || \
		(echo "Installing coverage tools..."; cargo install cargo-llvm-cov && rustup component add llvm-tools-preview)
	@cargo llvm-cov --version >/dev/null 2>&1 || (echo "Install failed. Run: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview"; exit 1)

.PHONY: coverage-unit
coverage-unit: coverage-deps ## Run unit/integration tests with coverage (excludes e2e_test)
	@echo "📊 Running unit/integration coverage..."
	export RUSTFLAGS="$(COVERAGE_RUSTFLAGS)" && \
	cargo llvm-cov test --workspace --exclude e2e_test --no-fail-fast --no-report
	export RUSTFLAGS="$(COVERAGE_RUSTFLAGS)" && \
	cargo llvm-cov report --lcov --output-path "$(COVERAGE_LCOV_PATH)"
	@echo "📄 LCOV (editor): $(COVERAGE_LCOV_PATH)"

.PHONY: coverage-e2e
coverage-e2e: coverage-deps ## Run e2e tests with coverage (e2e_test crate only)
	@echo "📊 Running e2e coverage..."
	@mkdir -p "$(COVERAGE_E2E_DIR)"
	export RUSTFLAGS="$(COVERAGE_RUSTFLAGS)" && \
	cargo llvm-cov test -p e2e_test --no-fail-fast --no-report
	export RUSTFLAGS="$(COVERAGE_RUSTFLAGS)" && \
	cargo llvm-cov report -p e2e_test --lcov --output-path "$(COVERAGE_E2E_DIR)/lcov.info"
	@echo "📄 LCOV: $(COVERAGE_E2E_DIR)/lcov.info"

.PHONY: coverage-combined
coverage-combined: coverage-deps ## Run all tests (unit + e2e) with a single combined coverage report
	@echo "📊 Running combined coverage (unit + e2e)..."
	export RUSTFLAGS="$(COVERAGE_RUSTFLAGS)" && \
	cargo llvm-cov test --workspace --no-fail-fast --no-report
	export RUSTFLAGS="$(COVERAGE_RUSTFLAGS)" && \
	cargo llvm-cov report --lcov --output-path "$(COVERAGE_LCOV_PATH)"
	@echo "📄 LCOV (editor): $(COVERAGE_LCOV_PATH)"
