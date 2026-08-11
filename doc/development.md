# Development Guide

This page covers building, testing, and running quality checks for RustFS. For code formatting rules and PR expectations, see [CONTRIBUTING.md](../CONTRIBUTING.md).

## Prerequisites

- Rust (see root [Cargo.toml](../Cargo.toml) `rust-version`)
- For full quality gates: `make` (see [.config/make/](../.config/make/) and root [Makefile](../Makefile))

## Build

```bash
# Build the RustFS binary
make build
# or
cargo build --release
```

## Testing

```bash
# Run tests
make test
# or
cargo test --all-targets
```

## Code Quality (Mandatory Before Commit)

Run all pre-commit checks:

```bash
make pre-commit
```

If `make` is unavailable, run the equivalent steps from [.config/make/](../.config/make/). Typical steps:

- Format: `cargo fmt --all` and `cargo fmt --all --check`
- Lint: `cargo clippy --all-targets --all-features -- -D warnings`
- Check: `cargo check --all-targets`
- Tests: `cargo test --all-targets`

See [CONTRIBUTING.md](../CONTRIBUTING.md) for detailed formatting and clippy rules.

## Make Targets

- `make help` — Main help and categories
- `make help-build` — Build-related targets
- `make help-docker` — Docker and image targets
- `make fmt` / `make fmt-check` — Format and verify
- `make clippy` — Clippy
- `make check` — Compilation check
- `make test` — Tests
- `make pre-commit` — All checks required before commit
- `make doc` — Generate Rust API documentation (HTML in `target/doc`)
- `make doc-serve` — Generate docs and serve at http://127.0.0.1:8765/ (all links work; Ctrl+C to stop)
- `make doc-open` — Generate docs, start local server, and open browser (use this so cross-crate links work)

## Coverage

Coverage uses `cargo-llvm-cov`. Install once:

```bash
cargo install cargo-llvm-cov
rustup component add llvm-tools-preview
```

Then run:

- `make coverage-unit` — Unit and integration tests (all crates except `e2e_test`). Writes LCOV to `lcov.info` (repo root).
- `make coverage-e2e` — E2E tests only (`e2e_test` crate). Writes LCOV to `target/coverage/e2e/lcov.info`.
- `make coverage-combined` — All tests in one run. Writes LCOV to `lcov.info` (repo root).

Use the LCOV file in Cursor or VS Code with a coverage extension (e.g. Coverage Gutters) for inline coverage; the extension looks for `lcov.info` in the workspace by default. Override the path with `COVERAGE_LCOV_PATH`, or for e2e use `COVERAGE_OUTPUT_DIR`.

Doctests are not included in coverage (they require the nightly toolchain).

Coverage builds use the BFD linker (`-fuse-ld=bfd`) by default to avoid LLD duplicate-symbol errors with `-C instrument-coverage` (e.g. with datafusion/arrow deps). This requires binutils (e.g. `ld.bfd`) on the system. To use another linker, set `COVERAGE_RUSTFLAGS` when invoking the target, e.g. `make coverage-unit COVERAGE_RUSTFLAGS=`.

## CI

Quality gates are defined in [.github/workflows/ci.yml](../.github/workflows/ci.yml). Keep local checks aligned with CI so `make pre-commit` matches what runs on the branch.

## Branch and PR Baseline

- Use feature branches from latest `main`.
- Follow [Conventional Commits](https://www.conventionalcommits.org/), subject ≤ 72 characters.
- Use [.github/pull_request_template.md](../.github/pull_request_template.md) for PRs; use `N/A` for non-applicable sections.
