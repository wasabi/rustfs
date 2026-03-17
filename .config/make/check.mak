## —— Check and Inform Dependencies ----------------------------------------------------------------

# Fatal check
# Checks all required dependencies and exits with error if not found
# (e.g., cargo, rustfmt)
check-%:
	@command -v $* >/dev/null 2>&1 || { \
		echo >&2 "❌ '$*' is not installed."; \
		exit 1; \
	}

# Warning-only check
# Checks for optional dependencies and issues a warning if not found
# (e.g., cargo-nextest for enhanced testing)
warn-%:
	@command -v $* >/dev/null 2>&1 || { \
		echo >&2 "⚠️ '$*' is not installed."; \
	}

# For checking dependencies use check-<dep-name> or warn-<dep-name>
.PHONY: core-deps fmt-deps test-deps check-awscurl install-awscurl ensure-awscurl
core-deps: check-cargo ## Check core dependencies
fmt-deps: check-rustfmt ## Check lint and formatting dependencies
test-deps: warn-cargo-nextest ## Check tests dependencies

# awscurl: required for e2e tests that call admin/HTTP endpoints (e.g. quota_test)
# Check only; use ensure-awscurl to install if missing.
check-awscurl:
	@if [ -n "$${AWSCURL_PATH:-}" ] && [ -x "$${AWSCURL_PATH:-}" ]; then \
		echo "✅ awscurl found at $${AWSCURL_PATH}"; \
	elif command -v awscurl >/dev/null 2>&1; then \
		echo "✅ awscurl found ($$(command -v awscurl))"; \
	else \
		echo >&2 "❌ awscurl is not installed and AWSCURL_PATH is not set."; \
		echo >&2 "   Install with: make install-awscurl"; \
		exit 1; \
	fi

# Install awscurl: prefer pipx (avoids externally-managed-environment); fallback to pip --user.
install-awscurl:
	@if [ -n "$${AWSCURL_PATH:-}" ] && [ -x "$${AWSCURL_PATH:-}" ]; then \
		echo "✅ awscurl already available at $${AWSCURL_PATH}"; \
	elif command -v awscurl >/dev/null 2>&1; then \
		echo "✅ awscurl already installed ($$(command -v awscurl))"; \
	else \
		echo "Installing awscurl..."; \
		if command -v pipx >/dev/null 2>&1 && pipx install awscurl; then \
			:; \
		elif command -v pip3 >/dev/null 2>&1 && pip3 install --user awscurl; then \
			:; \
		elif command -v pip >/dev/null 2>&1 && pip install --user awscurl; then \
			:; \
		else \
			echo >&2 "❌ Could not install awscurl."; \
			echo >&2 "   On externally-managed Python, use: pipx install awscurl"; \
			echo >&2 "   (Install pipx if needed: e.g. apt install pipx && pipx ensurepath)"; \
			exit 1; \
		fi; \
		echo "✅ awscurl installed ($$(command -v awscurl))"; \
	fi

# Idempotent: ensure awscurl is available, installing if missing.
ensure-awscurl: install-awscurl
