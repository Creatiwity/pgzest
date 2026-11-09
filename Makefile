# pgzest — development
#
# Single entry point. `make help` lists the targets.
# Cold start:  make bootstrap && make demo

SHELL := /bin/bash
CARGO ?= cargo
NPM   ?= npm

NPM_PKG   := npm/pgzest
VSCODE    := editors/vscode
CLI       := ./target/debug/pgzest
EXAMPLES  := examples

.DEFAULT_GOAL := help

# ── Help ─────────────────────────────────────────────────────────────────────

.PHONY: help
help: ## Show this help
	@echo "pgzest — available targets:"
	@echo
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | sort \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo "Cold start:  make bootstrap && make demo"

# ── Bootstrap ────────────────────────────────────────────────────────────────

.PHONY: bootstrap
bootstrap: ## Install all dependencies (Rust + npm) and check the tooling
	@echo "==> Checking the tooling"
	@command -v $(CARGO) >/dev/null || { echo "cargo not found — install Rust: https://rustup.rs"; exit 1; }
	@$(CARGO) --version
	@command -v $(NPM) >/dev/null || { echo "npm not found — install Node.js >= 18"; exit 1; }
	@$(NPM) --version
	@command -v wasm-pack >/dev/null \
	  || echo "  (!) wasm-pack missing — 'make wasm' will fail. Install it: cargo install wasm-pack"
	@echo "==> npm install: $(NPM_PKG)"
	@cd $(NPM_PKG) && $(NPM) install --no-fund --no-audit
	@echo "==> npm install: $(VSCODE)"
	@cd $(VSCODE) && $(NPM) install --no-fund --no-audit
	@echo "==> Bootstrap done. Next step: make demo"

# ── Build ────────────────────────────────────────────────────────────────────

.PHONY: build
build: ## Build all Rust crates (debug)
	$(CARGO) build --workspace

.PHONY: release
release: ## Build all Rust crates (release)
	$(CARGO) build --workspace --release

.PHONY: ts
ts: ## Build the pgzest npm package -> dist/ (frozen until P8, out of CI)
	cd $(NPM_PKG) && npx tsc

.PHONY: vscode
vscode: ## Build the VS Code extension -> out/ (typecheck + bundle)
	cd $(VSCODE) && $(NPM) run compile

.PHONY: wasm
wasm: ## Build the WASM bindings (requires wasm-pack)
	cd bindings/wasm && ./build.sh

.PHONY: all
all: build ts vscode ## Build Rust + TypeScript + extension

# ── Tests & quality ─────────────────────────────────────────────────────────

.PHONY: test
test: test-rust ## Run all tests (the frozen npm package: make test-ts)

.PHONY: test-rust
test-rust: ## Rust tests
	$(CARGO) test --workspace

.PHONY: test-ts
test-ts: ts ## TypeScript tests of the npm package (frozen until P8, out of CI)
	cd $(NPM_PKG) && node --test "dist/**/*.test.js"

.PHONY: clippy
clippy: ## Rust lints — blocking, as in CI
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: fmt
fmt: ## Format the Rust code
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Check formatting without modifying
	$(CARGO) fmt --all --check

.PHONY: typecheck
typecheck: ## Typecheck the VS Code extension without emitting
	cd $(VSCODE) && $(NPM) run typecheck

.PHONY: ci
ci: build test fmt-check clippy typecheck ## Exactly what CI checks

# ── Oracle PostgreSQL (PLAN.md §7) ───────────────────────────────────────────
#
# A disposable PostgreSQL 18 in Docker, on a dedicated port so as never to
# touch a local database. The oracle creates then drops one database per case.

ORACLE_CONTAINER := pgzest-oracle-pg18
ORACLE_PORT      := 54318
ORACLE_URL       := postgres://postgres:pgzest@localhost:$(ORACLE_PORT)/postgres

.PHONY: oracle-up
oracle-up: ## Start the oracle's PostgreSQL 18 (Docker)
	@command -v docker >/dev/null || { echo "docker not found"; exit 1; }
	@if [ -z "$$(docker ps -q -f name=^$(ORACLE_CONTAINER)$$)" ]; then \
	  docker rm -f $(ORACLE_CONTAINER) >/dev/null 2>&1 || true; \
	  docker run -d --name $(ORACLE_CONTAINER) -e POSTGRES_PASSWORD=pgzest \
	    -p $(ORACLE_PORT):5432 postgres:18 >/dev/null; \
	fi
	@for i in $$(seq 1 30); do \
	  docker exec $(ORACLE_CONTAINER) pg_isready -U postgres -h localhost >/dev/null 2>&1 && exit 0; \
	  sleep 1; \
	done; echo "PostgreSQL is not responding"; exit 1

.PHONY: oracle
oracle: oracle-up ## Check the analysis against PostgreSQL 18 (report: crates/pgzest-oracle/tests/snapshots)
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest-oracle --test oracle -- --ignored
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest-sema --test oracle_sweep -- --ignored
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest-oracle --test precedence -- --ignored
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest-oracle --test migrations -- --ignored
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest-oracle --test instrument -- --ignored
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest --test dev -- --ignored
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest --test db_check -- --ignored
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest --test migrate_apply -- --ignored
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest-runtime --features opentelemetry --test trace -- --ignored
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgzest-example-rust-service -- --ignored

.PHONY: bench
bench: oracle-up ## Cost of the generated code vs hand-written tokio-postgres (report printed)
	PGZEST_ORACLE_URL=$(ORACLE_URL) $(CARGO) test --release -p pgzest-example-rust-service --test bench -- --ignored --nocapture

.PHONY: oracle-accept
oracle-accept: oracle-up ## Like `oracle`, accepting the new report (review before committing)
	PGZEST_ORACLE_URL=$(ORACLE_URL) INSTA_UPDATE=always $(CARGO) test -p pgzest-oracle --test oracle -- --ignored

.PHONY: oracle-down
oracle-down: ## Stop and remove the oracle's PostgreSQL
	docker rm -f $(ORACLE_CONTAINER)

# ── Demo / smoke ─────────────────────────────────────────────────────────────

.PHONY: demo-cli
demo-cli: build ## Run the CLI on examples/ — shows the tool in action
	@echo
	@echo "════ pgzest check examples/test.sql ════"
	@echo "   (expected error: nonexistent table)"
	@$(CLI) check $(EXAMPLES)/test.sql || true
	@echo
	@echo "════ pgzest check examples/schema.sql ════"
	@$(CLI) check $(EXAMPLES)/schema.sql || true
	@echo
	@echo "════ pgzest lint examples/schema.sql ════"
	@$(CLI) lint $(EXAMPLES)/schema.sql || true
	@echo
	@echo "════ pgzest emit examples/test.sql ════"
	@$(CLI) emit $(EXAMPLES)/test.sql || true
	@echo
	@echo "Demo done. The CLI is at $(CLI) — see 'make help'."

.PHONY: demo
demo: ## Open the demo (examples/demo) in VS Code, pgzest extension in development mode
	$(CARGO) build -p pgzest
	$(MAKE) vscode
	code --extensionDevelopmentPath=$(CURDIR)/$(VSCODE) $(CURDIR)/examples/demo
	@echo
	@echo "VS Code window opened on examples/demo: follow examples/demo/README.md."

.PHONY: demo-run
demo-run: oracle-up ## Run the demo's Rust scenario on PostgreSQL 18 (Docker)
	$(CARGO) run -p pgzest-demo

.PHONY: lsp
lsp: ## Build pgzest (its `lsp` subcommand is the language server) and recall how to launch it in VS Code
	$(CARGO) build -p pgzest
	@echo
	@echo "Language server: ./target/debug/pgzest lsp"
	@echo "To try it:  make vscode-dev"

.PHONY: vscode-dev
vscode-dev: lsp vscode ## Prepare everything to test in VS Code, then explain F5
	@echo
	@echo "════════════════════════════════════════════════════════════════"
	@echo " Everything is built. To test in VS Code:"
	@echo
	@echo "   code $(VSCODE)"
	@echo "   then F5  →  \"Run Extension (playground)\""
	@echo
	@echo " A window opens on examples/playground/:"
	@echo "   queries/01-ok.sql          must be clean"
	@echo "   queries/02-errors.sql      nonexistent columns/tables"
	@echo "   queries/03-directives.sql  ESLint-style directives"
	@echo "   queries/04-tolerance-*.sql unreadable statement as a warning"
	@echo "   queries/05-lint.sql        lint rules (LSP only)"
	@echo
	@echo " Details: examples/playground/README.md"
	@echo "════════════════════════════════════════════════════════════════"

.PHONY: playground
playground: build ## Run the CLI on the demo project
	@cd $(EXAMPLES)/playground && for f in queries/*.sql; do \
	  echo "════ pgzest check $$f ════"; \
	  ../../$(CLI) check "$$f" || true; \
	done; \
	echo "════ pgzest lint queries/05-lint.sql ════"; \
	../../$(CLI) lint queries/05-lint.sql || true

# ── Documentation site ──────────────────────────────────────────────────────

.PHONY: docs
docs: ## Regenerate the site's CLI and diagnostics references from the code
	$(CARGO) run -q -p xtask -- docs

.PHONY: site
site: docs ## Build the documentation site (site/dist), checking its links
	cd site && npm install --no-fund --no-audit && npm run build

.PHONY: site-dev
site-dev: docs ## Serve the documentation site locally, reloading on edits
	cd site && npm install --no-fund --no-audit && npm run dev

# ── Cleanup ─────────────────────────────────────────────────────────────────

.PHONY: clean
clean: ## Remove build artifacts
	$(CARGO) clean
	rm -rf $(NPM_PKG)/dist $(VSCODE)/out $(VSCODE)/server $(VSCODE)/*.vsix bindings/wasm/pkg

.PHONY: distclean
distclean: clean ## clean + remove node_modules
	rm -rf $(NPM_PKG)/node_modules $(VSCODE)/node_modules site/node_modules site/dist
