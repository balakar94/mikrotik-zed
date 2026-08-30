SHELL := /bin/bash
.SHELLFLAGS := -eu -c
MAKEFLAGS += --warn-undefined-variables

# ── Variables (override with `make VAR=value`) ───────────────────
GRAMMAR_DIR ?= grammars/rsc
WASM_TARGET ?= wasm32-wasip2
WASM_CRATE  ?= mikrotik_zed
WASM_OUT    ?= target/$(WASM_TARGET)/release/$(WASM_CRATE).wasm
VENV_DIR    ?= .venv
PYTHON      ?= $(shell [ -x .venv/bin/python ] && echo .venv/bin/python || echo python3)
SKIP_SYSTEM ?=
FILE        ?=
VERSION     ?=

.PHONY: help generate generate-check test-grammar test-rust test-python test-all grammar-clone parse highlight extract sync sync-check check-manifest build build-lsp check check-wasm check-lsp fmt fmt-fix clippy audit install install-deps install-tools install-lsp install-dev bump clean clean-generated validate validate-fast _check-tools _clean-artifacts

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

# ── Internal helpers (hidden from help) ──────────────────────────
_check-tools:
	@command -v cargo >/dev/null || (echo "error: cargo not found — run 'make install'" && false)
	@command -v rustup >/dev/null && rustup target list --installed | grep -q $(WASM_TARGET) || echo "hint: rustup target add $(WASM_TARGET)"
	@command -v npx >/dev/null || echo "hint: npm install -g tree-sitter-cli (for grammar)"
	@command -v $(PYTHON) >/dev/null || echo "hint: $(PYTHON) not found — Python 3.11+ needed (run 'make install-tools')"

# ── Tree-sitter grammar ──────────────────────────────────────────
generate: ## Regenerate parser.c from grammar.js (requires tree-sitter-cli)
	@command -v npx >/dev/null || (echo "error: npx not found — npm install -g tree-sitter-cli" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter generate

generate-check: ## Verify parser.c is up-to-date (CI: fails if stale)
	@command -v npx >/dev/null || (echo "error: npx not found" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter generate
	git -C $(GRAMMAR_DIR) diff --exit-code -- src/parser.c src/grammar.json src/node-types.json || \
		(echo "parser.c stale — run 'make generate' and commit" && false)

test-grammar: ## Run tree-sitter grammar corpus tests
	@command -v npx >/dev/null || (echo "error: npx not found" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter test

test-rust: ## Run Rust tests (all workspace members)
	cargo test --workspace

test-python: ## Run Python test suite
	@command -v $(PYTHON) >/dev/null || (echo "skip: $(PYTHON) not found" && exit 0)
	@command -v pytest >/dev/null 2>&1 || $(PYTHON) -m pytest --version >/dev/null 2>&1 || (echo "skip: pytest not found (pip install pytest)" && exit 0)
	$(PYTHON) -m pytest tests/ -v

test-all: test-grammar test-rust test-python ## Run all tests
grammar-clone: ## Clone grammar working copy at rev pinned in extension.toml
	@if [ -d $(GRAMMAR_DIR)/.git ] || [ -f $(GRAMMAR_DIR)/.git ]; then \
		echo "grammars/rsc already present"; \
	else \
		git clone https://github.com/balakar94/tree-sitter-rsc $(GRAMMAR_DIR); \
	fi
	@REV=$$(awk '/^\[grammars\.rsc\]/{f=1;next} f&&/^rev/{gsub(/"/,"",$$3);print $$3;exit}' extension.toml); \
	if [ -n "$$REV" ] && [ "$$(git -C $(GRAMMAR_DIR) rev-parse HEAD)" != "$$REV" ]; then \
		echo "$$REV" | grep -Eq '^[0-9a-f]{40}$$' || (echo "error: REV must be 40-char hex" && false); \
		git -C $(GRAMMAR_DIR) fetch --depth 1 origin "$$REV" && git -C $(GRAMMAR_DIR) checkout --detach FETCH_HEAD; fi
	@git -C $(GRAMMAR_DIR) log --oneline -1
parse: ## Parse a file (usage: make parse FILE=path/to/file.rsc)
	@test -n "$(FILE)" || (echo "usage: make parse FILE=path/to/file.rsc" && false)
	@test -f "$(FILE)" || (echo "error: file not found: $(FILE)" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter parse ../../$(FILE)
highlight: ## Highlight a file (usage: make highlight FILE=path/to/file.rsc)
	@test -n "$(FILE)" || (echo "usage: make highlight FILE=path/to/file.rsc" && false)
	@test -f "$(FILE)" || (echo "error: file not found: $(FILE)" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter highlight ../../$(FILE)
# ── Command extraction ───────────────────────────────────────────
extract: ## Regenerate data/commands.toml from llms-full.txt
	@test -f llms-full.txt || (echo "error: llms-full.txt not found — run 'make sync'" && false)
	@command -v $(PYTHON) >/dev/null || (echo "error: $(PYTHON) not found" && false)
	$(PYTHON) scripts/extract_commands.py
sync: ## Fetch latest llms.txt and llms-full.txt from upstream
	@command -v $(PYTHON) >/dev/null || (echo "error: $(PYTHON) not found" && false)
	$(PYTHON) scripts/sync_llms.py
sync-check: ## Check if llms files are stale vs upstream (CI gate)
	@command -v $(PYTHON) >/dev/null || (echo "error: $(PYTHON) not found" && false)
	$(PYTHON) scripts/sync_llms.py --check
check-manifest: ## Check extension against Zed requirements (manifest + registry policy)
	@command -v $(PYTHON) >/dev/null || (echo "error: $(PYTHON) not found" && false)
	$(PYTHON) scripts/check_zed_requirements.py
# ── Build ────────────────────────────────────────────────────────
build: ## Build WASM extension (wasm32-wasip2 component) and stage extension.wasm
	cargo build --target $(WASM_TARGET) --release
	cp $(WASM_OUT) extension.wasm
	@echo "WASM component: extension.wasm ($$(stat -f%z extension.wasm 2>/dev/null || stat -c%s extension.wasm) bytes)"
	@echo "Note: Rust 1.84+ emits a component directly for wasm32-wasip2; no wasm-tools step needed."
build-lsp: ## Build native LSP binary (target/release/rsc-ls)
	cargo build -p rsc-ls --release
	@echo "Binary: target/release/rsc-ls ($$(stat -f%z target/release/rsc-ls 2>/dev/null || stat -c%s target/release/rsc-ls) bytes)"
check: check-wasm check-lsp ## Quick compile verification (wasm + lsp)
check-wasm: ## Check WASM extension compiles
	cargo check --target $(WASM_TARGET)
check-lsp: ## Check LSP binary compiles
	cargo check -p rsc-ls
fmt: ## Format Rust code (check)
	cargo fmt --all -- --check
	@echo "fmt ok (use 'make fmt-fix' to fix)"
fmt-fix: ## Format Rust code (write)
	cargo fmt --all

clippy: ## Lint with clippy (wasm + native, -D warnings)
	cargo clippy --target $(WASM_TARGET) -- -D warnings
	cargo clippy -p rsc-ls --all-targets -- -D warnings

audit: ## Audit dependencies (requires cargo-audit)
	@command -v cargo-audit >/dev/null 2>&1 || (echo "install: cargo install cargo-audit" && false)
	cargo audit

# ── Install / Bootstrap ──────────────────────────────────────────
install-deps: ## Install system dependencies for detected platform
	@if [ "$(SKIP_SYSTEM)" = "1" ]; then echo "skip: system packages (SKIP_SYSTEM=1)"; exit 0; fi
	@uname_s=$$(uname -s); sudo=""; [ "$$(id -u)" -eq 0 ] || sudo="sudo"; \
	dnf=""; command -v dnf >/dev/null 2>&1 && dnf=$$(command -v dnf) || dnf=$$(command -v yum 2>/dev/null || true); \
	apt=""; command -v apt-get >/dev/null 2>&1 && apt=$$(command -v apt-get) || true; \
	if [ "$$uname_s" = "Darwin" ]; then \
		echo "==> [macOS] installing system dependencies"; \
		command -v brew >/dev/null 2>&1 || { echo "error: Homebrew not found — https://brew.sh" >&2; exit 1; }; \
		xcode-select -p >/dev/null 2>&1 || { echo "error: Xcode CLT missing — xcode-select --install" >&2; exit 1; }; \
		echo "brew install python node"; brew install python node; \
	elif [ -n "$$dnf" ]; then \
		echo "==> [Fedora/RHEL] installing system dependencies"; \
		$$sudo $$dnf install -y gcc gcc-c++ make curl git ca-certificates openssl-devel pkgconf-pkg-config python3 python3-pip nodejs npm; \
	elif command -v pacman >/dev/null 2>&1; then \
		echo "==> [Arch] installing system dependencies"; \
		$$sudo pacman -Sy --needed --noconfirm base-devel curl git python python-pip nodejs npm; \
	elif [ -n "$$apt" ]; then \
		echo "==> [Debian/Ubuntu] installing system dependencies"; \
		$$sudo apt-get update && $$sudo $$apt install -y build-essential curl git ca-certificates pkg-config libssl-dev python3 python3-venv python3-pip nodejs npm; \
	else echo "error: unsupported platform — see README Dependencies" >&2; exit 1; fi

install-tools: ## Install toolchains: rustup+wasm32-wasip2, Python venv (.venv) with deps, tree-sitter-cli
	@if ! command -v cargo >/dev/null 2>&1; then \
		echo "==> Installing rustup (minimal profile)"; \
		curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal; \
		. "$$HOME/.cargo/env"; \
	fi
	@if [ -f "$$HOME/.cargo/env" ]; then . "$$HOME/.cargo/env"; fi; rustup target add $(WASM_TARGET)
	@if [ ! -x "$(VENV_DIR)/bin/python" ]; then \
		echo "==> Creating Python venv at $(VENV_DIR)"; \
		python3 -m venv $(VENV_DIR); \
	fi
	$(VENV_DIR)/bin/pip install --upgrade pip pytest requests paramiko
	@if [ ! -d "$(GRAMMAR_DIR)" ]; then \
		echo "hint: $(GRAMMAR_DIR) not present — skipping tree-sitter-cli (run 'make grammar-clone')"; \
	elif command -v npm >/dev/null 2>&1; then \
		echo "==> Installing tree-sitter-cli in $(GRAMMAR_DIR)"; \
		cd $(GRAMMAR_DIR) && npm install --ignore-scripts && \
		{ [ -x node_modules/tree-sitter-cli/tree-sitter ] || (cd node_modules/tree-sitter-cli && node install.js); }; \
	else \
		echo "hint: npm not found — skipping tree-sitter-cli (install Node.js)"; \
	fi
	@echo "==> Toolchains ready: cargo + $(WASM_TARGET), $(VENV_DIR)/bin/python, tree-sitter-cli via npx"
	@echo "    Activate venv: source $(VENV_DIR)/bin/activate"

install-lsp: build-lsp ## Build rsc-ls and copy to PATH (~/.cargo/bin; + /opt/homebrew/bin on macOS, ~/.local/bin on Linux)
	@mkdir -p ~/.cargo/bin; cp target/release/rsc-ls ~/.cargo/bin/rsc-ls; chmod +x ~/.cargo/bin/rsc-ls
	@echo "Installed: ~/.cargo/bin/rsc-ls"
	@uname_s=$$(uname -s); \
	if [ "$$uname_s" = "Darwin" ] && [ -d /opt/homebrew/bin ]; then cp target/release/rsc-ls /opt/homebrew/bin/rsc-ls && echo "Installed: /opt/homebrew/bin/rsc-ls (for Zed GUI)"; fi; \
	if [ "$$uname_s" != "Darwin" ] && [ -d "$$HOME/.local/bin" ]; then cp target/release/rsc-ls "$$HOME/.local/bin/rsc-ls" && echo "Installed: $$HOME/.local/bin/rsc-ls"; fi
	@echo "Verify: which rsc-ls && rsc-ls --help 2>&1 | head -n 5 || echo 'rsc-ls ready'"

install: install-deps install-tools install-lsp ## Full bootstrap: system deps + toolchains + rsc-ls (SKIP_SYSTEM=1 to skip distro packages)
	@echo "==> Bootstrap complete."

bump: ## Bump version (usage: make bump VERSION=0.2.0)
	@test -n "$(VERSION)" || (echo "usage: make bump VERSION=0.2.0" && false)
	@echo "$(VERSION)" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$$' || (echo "error: VERSION must be semver x.y.z" && false)
	@echo "Bumping to $(VERSION) ..."
	@python3 -c 'import re,pathlib,sys; v=sys.argv[1]; [pathlib.Path(p).write_text(re.sub(r"^version = \".*\"", f"version = \"{v}\"", pathlib.Path(p).read_text(), flags=re.M)) for p in ["Cargo.toml","lsp/Cargo.toml","extension.toml"]]' "$(VERSION)"
	@echo "Note: grammars/rsc versions live in the separate tree-sitter-rsc repo (untracked working copy)"
	@echo "      never bumped from here — publish_grammar.py handles that repo."
	@echo "Note: Cargo.lock refreshes on next cargo command — commit it with the bumps."
	@echo "Bumped. Now run: cargo check && git diff"

# ── Cleanup ──────────────────────────────────────────────────────
_clean-artifacts:
	rm -rf target/; rm -f extension.wasm
	cd $(GRAMMAR_DIR) && rm -f parser.dylib tree-sitter-rsc.wasm

clean: _clean-artifacts ## Remove build artifacts (preserves Cargo.lock and parser.c)
	cd $(GRAMMAR_DIR) && rm -rf target/ build/ node_modules/ 2>/dev/null || true

clean-generated: ## Remove ALL generated files including parser.c (asks confirmation)
	@read -p "Remove parser.c, grammar.json, node-types.json? [y/N] " confirm && [ "$$confirm" = "y" ] || (echo "aborted" && exit 1)
	@$(MAKE) --no-print-directory _clean-artifacts
	cd $(GRAMMAR_DIR) && rm -rf target/ build/ src/grammar.json src/node-types.json src/parser.c
	@echo "Note: parser.c removed — run 'make generate' to regenerate"

# ── Development ──────────────────────────────────────────────────
install-dev: ## Point Zed to this directory (manual: Install Dev Extension)
	@echo "Open Zed → Command Palette → 'Install Dev Extension' → select this directory"
	@echo "Make sure rsc-ls binary is in PATH: make build-lsp && make install-lsp"

validate: check-manifest generate-check fmt clippy test-all extract ## Offline gate (manifest, generate-check, fmt, clippy, tests, extract); run make sync-check separately for upstream drift
	@git diff --exit-code data/commands.toml || (echo "data/commands.toml stale — run 'make extract' and commit" && false)
	@echo "All checks passed. Ready to commit."

validate-fast: check-manifest fmt clippy test-rust ## Quick gate (manifest, fmt, clippy, Rust tests)
	@echo "Fast checks passed."
