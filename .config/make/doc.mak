## —— Documentation (cargo doc) ----------------------------------------------------------------

DOC_PORT ?= 8765
DOC_DIR  := target/doc

.PHONY: doc
doc: ## Generate Rust API documentation (HTML in target/doc)
	@echo "📚 Building Rust API documentation..."
	cargo doc --workspace --no-deps

.PHONY: doc-serve
doc-serve: doc ## Serve docs at http://127.0.0.1:$(DOC_PORT)/ (links work correctly)
	@echo "📖 Serving docs at http://127.0.0.1:$(DOC_PORT)/ (Ctrl+C to stop)"
	@cd $(DOC_DIR) && python3 -m http.server $(DOC_PORT)

.PHONY: doc-open
doc-open: doc ## Generate docs, serve locally, and open in browser (fixes broken file:// links)
	@echo "📖 Starting local doc server and opening browser..."
	@if [ ! -d "$(DOC_DIR)" ]; then echo "No $(DOC_DIR); run make doc"; exit 1; fi; \
	( cd $(DOC_DIR) && nohup python3 -m http.server $(DOC_PORT) </dev/null >/dev/null 2>&1 & ); \
	sleep 1; \
	(xdg-open "http://127.0.0.1:$(DOC_PORT)/rustfs/" 2>/dev/null || open "http://127.0.0.1:$(DOC_PORT)/rustfs/" 2>/dev/null) \
		|| echo "Open http://127.0.0.1:$(DOC_PORT)/rustfs/ in your browser"; \
	echo "Docs: http://127.0.0.1:$(DOC_PORT)/ — stop server with: pkill -f 'python3 -m http.server $(DOC_PORT)'"
