# pgweave — développement
#
# Point d'entrée unique. `make help` liste les cibles.
# Démarrage à froid :  make bootstrap && make demo

SHELL := /bin/bash
CARGO ?= cargo
NPM   ?= npm

NPM_PKG   := npm/pgweave
VSCODE    := editors/vscode
CLI       := ./target/debug/pgweave
EXAMPLES  := examples

.DEFAULT_GOAL := help

# ── Aide ─────────────────────────────────────────────────────────────────────

.PHONY: help
help: ## Affiche cette aide
	@echo "pgweave — cibles disponibles :"
	@echo
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | sort \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo "Démarrage à froid :  make bootstrap && make demo"

# ── Bootstrap ────────────────────────────────────────────────────────────────

.PHONY: bootstrap
bootstrap: ## Installe toutes les dépendances (Rust + npm) et vérifie l'outillage
	@echo "==> Vérification de l'outillage"
	@command -v $(CARGO) >/dev/null || { echo "cargo introuvable — installe Rust: https://rustup.rs"; exit 1; }
	@$(CARGO) --version
	@command -v $(NPM) >/dev/null || { echo "npm introuvable — installe Node.js >= 18"; exit 1; }
	@$(NPM) --version
	@command -v wasm-pack >/dev/null \
	  || echo "  (!) wasm-pack absent — 'make wasm' échouera. Installe-le: cargo install wasm-pack"
	@echo "==> npm install : $(NPM_PKG)"
	@cd $(NPM_PKG) && $(NPM) install --no-fund --no-audit
	@echo "==> npm install : $(VSCODE)"
	@cd $(VSCODE) && $(NPM) install --no-fund --no-audit
	@echo "==> Bootstrap terminé. Prochaine étape : make demo"

# ── Build ────────────────────────────────────────────────────────────────────

.PHONY: build
build: ## Compile tous les crates Rust (debug)
	$(CARGO) build --workspace

.PHONY: release
release: ## Compile tous les crates Rust (release)
	$(CARGO) build --workspace --release

.PHONY: ts
ts: ## Compile le paquet npm pgweave -> dist/
	cd $(NPM_PKG) && npx tsc

.PHONY: vscode
vscode: ## Compile l'extension VS Code -> out/
	cd $(VSCODE) && npx tsc -p ./

.PHONY: wasm
wasm: ## Compile les bindings WASM (nécessite wasm-pack)
	cd bindings/wasm && ./build.sh

.PHONY: all
all: build ts vscode ## Compile Rust + TypeScript + extension

# ── Tests & qualité ──────────────────────────────────────────────────────────

.PHONY: test
test: test-rust test-ts ## Lance tous les tests

.PHONY: test-rust
test-rust: ## Tests Rust
	$(CARGO) test --workspace

.PHONY: test-ts
test-ts: ts ## Tests TypeScript du paquet npm
	cd $(NPM_PKG) && node --test "dist/**/*.test.js"

.PHONY: clippy
clippy: ## Lints Rust — bloquant, comme en CI
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: fmt
fmt: ## Formate le code Rust
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Vérifie le formatage sans modifier
	$(CARGO) fmt --all --check

.PHONY: typecheck
typecheck: ## Typecheck TypeScript sans émettre
	cd $(NPM_PKG) && npx tsc --noEmit
	cd $(VSCODE) && npx tsc -p ./ --noEmit

.PHONY: ci
ci: build test fmt-check clippy typecheck ## Ce que la CI vérifie, à l'identique

# ── Oracle PostgreSQL (PLAN.md §7) ───────────────────────────────────────────
#
# Un PostgreSQL 18 jetable dans Docker, sur un port dédié pour ne jamais
# toucher une base locale. L'oracle y crée puis supprime une base par cas.

ORACLE_CONTAINER := pgweave-oracle-pg18
ORACLE_PORT      := 54318
ORACLE_URL       := postgres://postgres:pgweave@localhost:$(ORACLE_PORT)/postgres

.PHONY: oracle-up
oracle-up: ## Démarre le PostgreSQL 18 de l'oracle (Docker)
	@command -v docker >/dev/null || { echo "docker introuvable"; exit 1; }
	@if [ -z "$$(docker ps -q -f name=^$(ORACLE_CONTAINER)$$)" ]; then \
	  docker rm -f $(ORACLE_CONTAINER) >/dev/null 2>&1 || true; \
	  docker run -d --name $(ORACLE_CONTAINER) -e POSTGRES_PASSWORD=pgweave \
	    -p $(ORACLE_PORT):5432 postgres:18 >/dev/null; \
	fi
	@for i in $$(seq 1 30); do \
	  docker exec $(ORACLE_CONTAINER) pg_isready -U postgres -h localhost >/dev/null 2>&1 && exit 0; \
	  sleep 1; \
	done; echo "PostgreSQL ne répond pas"; exit 1

.PHONY: oracle
oracle: oracle-up ## Confronte l'analyse à PostgreSQL 18 (rapport : crates/pgweave-oracle/tests/snapshots)
	PGWEAVE_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgweave-oracle --test oracle -- --ignored
	PGWEAVE_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgweave-sema --test oracle_sweep -- --ignored
	PGWEAVE_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgweave-oracle --test precedence -- --ignored
	PGWEAVE_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgweave-oracle --test migrations -- --ignored
	PGWEAVE_ORACLE_URL=$(ORACLE_URL) $(CARGO) test -p pgweave-example-rust-service -- --ignored

.PHONY: bench
bench: oracle-up ## Coût du code généré face à tokio-postgres écrit à la main (rapport affiché)
	PGWEAVE_ORACLE_URL=$(ORACLE_URL) $(CARGO) test --release -p pgweave-example-rust-service --test bench -- --ignored --nocapture

.PHONY: oracle-accept
oracle-accept: oracle-up ## Comme `oracle`, en acceptant le nouveau rapport (à relire avant commit)
	PGWEAVE_ORACLE_URL=$(ORACLE_URL) INSTA_UPDATE=always $(CARGO) test -p pgweave-oracle --test oracle -- --ignored

.PHONY: oracle-down
oracle-down: ## Arrête et supprime le PostgreSQL de l'oracle
	docker rm -f $(ORACLE_CONTAINER)

# ── Démo / smoke ─────────────────────────────────────────────────────────────

.PHONY: demo-cli
demo-cli: build ## Exécute la CLI sur examples/ — montre l'outil en action
	@echo
	@echo "════ pgweave check examples/test.sql ════"
	@echo "   (erreur attendue : table inexistante)"
	@$(CLI) check $(EXAMPLES)/test.sql || true
	@echo
	@echo "════ pgweave check examples/schema.sql ════"
	@$(CLI) check $(EXAMPLES)/schema.sql || true
	@echo
	@echo "════ pgweave lint examples/schema.sql ════"
	@$(CLI) lint $(EXAMPLES)/schema.sql || true
	@echo
	@echo "════ pgweave codegen --input examples/schema.sql ════"
	@$(CLI) codegen --input $(EXAMPLES)/schema.sql || true
	@echo
	@echo "════ pgweave emit examples/test.sql ════"
	@$(CLI) emit $(EXAMPLES)/test.sql || true
	@echo
	@echo "Démo terminée. La CLI est dans $(CLI) — voir 'make help'."

.PHONY: demo
demo: ## Ouvre la démo (examples/demo) dans VS Code, extension pgweave en mode développement
	$(CARGO) build -p pgweave-lsp -p pgweave
	$(MAKE) vscode
	code --extensionDevelopmentPath=$(CURDIR)/$(VSCODE) $(CURDIR)/examples/demo
	@echo
	@echo "Fenêtre VS Code ouverte sur examples/demo : suivre examples/demo/README.md."

.PHONY: demo-run
demo-run: oracle-up ## Exécute le scénario Rust de la démo sur PostgreSQL 18 (Docker)
	$(CARGO) run -p pgweave-demo

.PHONY: lsp
lsp: ## Compile le serveur LSP et rappelle comment le lancer dans VS Code
	$(CARGO) build -p pgweave-lsp
	@echo
	@echo "Binaire LSP : ./target/debug/pgweave-lsp"
	@echo "Pour l'essayer :  make vscode-dev"

.PHONY: vscode-dev
vscode-dev: lsp vscode ## Prépare tout pour tester dans VS Code, puis explique F5
	@echo
	@echo "════════════════════════════════════════════════════════════════"
	@echo " Tout est compilé. Pour tester dans VS Code :"
	@echo
	@echo "   code $(VSCODE)"
	@echo "   puis F5  →  « Run Extension (playground) »"
	@echo
	@echo " Une fenêtre s'ouvre sur examples/playground/ :"
	@echo "   queries/01-ok.sql          doit être propre"
	@echo "   queries/02-erreurs.sql     colonnes/tables inexistantes"
	@echo "   queries/03-directives.sql  directives ESLint-style"
	@echo "   queries/04-tolerance-*.sql statement illisible en warning"
	@echo "   queries/05-lint.sql        règles de lint (LSP uniquement)"
	@echo
	@echo " Détails : examples/playground/README.md"
	@echo "════════════════════════════════════════════════════════════════"

.PHONY: playground
playground: build ## Lance la CLI sur le projet de démonstration
	@cd $(EXAMPLES)/playground && for f in queries/*.sql; do \
	  echo "════ pgweave check $$f ════"; \
	  ../../$(CLI) check "$$f" || true; \
	done; \
	echo "════ pgweave lint queries/05-lint.sql ════"; \
	../../$(CLI) lint queries/05-lint.sql || true

# ── Nettoyage ────────────────────────────────────────────────────────────────

.PHONY: clean
clean: ## Supprime les artefacts de build
	$(CARGO) clean
	rm -rf $(NPM_PKG)/dist $(VSCODE)/out bindings/wasm/pkg

.PHONY: distclean
distclean: clean ## clean + supprime les node_modules
	rm -rf $(NPM_PKG)/node_modules $(VSCODE)/node_modules
