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
PIP         ?= pip3
SKIP_SYSTEM ?=
# Pre-define optional target inputs so --warn-undefined-variables stays quiet
# when these targets run without overrides (recipes validate them themselves).
FILE        ?=
VERSION     ?=

# ── Platform helpers (read-only shell probes at parse time) ──────
UNAME_S   := $(shell uname -s)
IS_DARWIN := $(findstring Darwin,$(UNAME_S))
SUDO      := $(shell [ "$$(id -u)" -eq 0 ] || echo sudo)
DNF       := $(shell command -v dnf 2>/dev/null || command -v yum 2>/dev/null)
APT       := $(shell command -v apt-get 2>/dev/null)

.PHONY: help generate test-grammar test-rust test-python test-all parse highlight extract sync sync-check check-manifest build build-lsp build-wasm check check-wasm check-lsp fmt fmt-fix clippy audit clean clean-artifacts clean-generated install install-deps install-tools install-lsp install-dev validate check-tools bump

# Default target
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

# ── Tooling checks ───────────────────────────────────────────────

check-tools: ## Verify required tools are installed
	@command -v cargo >/dev/null || (echo "error: cargo not found — run 'make install' to bootstrap, or install rustup (the pinned toolchain from rust-toolchain.toml provides the compiler)" && false)
	@command -v rustup >/dev/null && rustup target list --installed | grep -q $(WASM_TARGET) || echo "hint: rustup target add $(WASM_TARGET)"
	@command -v npx >/dev/null || echo "hint: npm install -g tree-sitter-cli (for grammar)"
	@command -v $(PYTHON) >/dev/null || echo "hint: $(PYTHON) not found — Python 3.12+ needed for extract/sync/test-python (run 'make install-tools')"

# ── Tree-sitter grammar ────────────────────────────────────────

generate: ## Regenerate parser.c from grammar.js (requires tree-sitter-cli)
	@command -v npx >/dev/null || (echo "error: npx not found — npm install -g tree-sitter-cli" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter generate

generate-check: ## Verify parser.c is up-to-date (CI: fails if stale)
	@command -v npx >/dev/null || (echo "error: npx not found" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter generate
	git -C $(GRAMMAR_DIR) diff --exit-code -- src/parser.c src/grammar.json src/node-types.json || \
		(echo "parser.c stale — run 'make generate' and commit" && false)

# NOTE: there is deliberately no `make test` alias — use test-all (everything)
# or the individual test-grammar / test-rust / test-python targets.

test-grammar: ## Run tree-sitter grammar corpus tests
	@command -v npx >/dev/null || (echo "error: npx not found" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter test

test-rust: ## Run Rust tests (all workspace members)
	# `--workspace` covers both members: the root crate's unit tests
	# (wasm extension) and lsp (rsc-ls).
	cargo test --workspace

test-python: ## Run Python test suite
	@command -v $(PYTHON) >/dev/null || (echo "skip: $(PYTHON) not found" && exit 0)
	@command -v pytest >/dev/null 2>&1 || $(PYTHON) -m pytest --version >/dev/null 2>&1 || (echo "skip: pytest not found (pip install pytest)" && exit 0)
	$(PYTHON) -m pytest tests/ -v

test-all: test-grammar test-rust test-python ## Run all tests

grammar-clone: ## Clone the grammar working copy at the rev pinned in extension.toml
	@if [ -d $(GRAMMAR_DIR)/.git ] || [ -f $(GRAMMAR_DIR)/.git ]; then \
		echo "grammars/rsc already present"; \
	else \
		git clone https://github.com/balakar94/tree-sitter-rsc $(GRAMMAR_DIR); \
	fi
	@REV=$$(awk '/^\[grammars\.rsc\]/{f=1;next} f&&/^rev/{gsub(/"/,"",$$3);print $$3;exit}' extension.toml); \
	if [ -n "$$REV" ] && [ "$$(git -C $(GRAMMAR_DIR) rev-parse HEAD)" != "$$REV" ]; then \
		git -C $(GRAMMAR_DIR) fetch --depth 1 origin "$$REV" && \
		git -C $(GRAMMAR_DIR) checkout --detach FETCH_HEAD; fi
	@git -C $(GRAMMAR_DIR) log --oneline -1

parse: ## Parse a file (usage: make parse FILE=grammars/rsc/test/example.rsc)
	@test -n "$(FILE)" || (echo "usage: make parse FILE=path/to/file.rsc" && false)
	@test -f "$(FILE)" || (echo "error: file not found: $(FILE)" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter parse ../../$(FILE)

highlight: ## Highlight a file (usage: make highlight FILE=grammars/rsc/test/example.rsc)
	@test -n "$(FILE)" || (echo "usage: make highlight FILE=path/to/file.rsc" && false)
	@test -f "$(FILE)" || (echo "error: file not found: $(FILE)" && false)
	cd $(GRAMMAR_DIR) && npx tree-sitter highlight ../../$(FILE)

# ── Command extraction ─────────────────────────────────────────

extract: ## Regenerate data/commands.toml from llms-full.txt
	@test -f llms-full.txt || (echo "error: llms-full.txt not found — fetch it first with 'make sync'" && false)
	@command -v $(PYTHON) >/dev/null || (echo "error: $(PYTHON) not found — Python 3.12+ required for extraction" && false)
	$(PYTHON) scripts/extract_commands.py

sync: ## Fetch latest llms.txt and llms-full.txt from upstream (manual.mikrotik.com)
	@command -v $(PYTHON) >/dev/null || (echo "error: $(PYTHON) not found" && false)
	$(PYTHON) scripts/sync_llms.py

sync-check: ## Check if llms files are stale vs upstream (CI gate; standalone use — refresh with 'make sync')
	@command -v $(PYTHON) >/dev/null || (echo "error: $(PYTHON) not found" && false)
	$(PYTHON) scripts/sync_llms.py --check

check-manifest: ## Check the extension against Zed's requirements (manifest schema + registry policy)
	@command -v $(PYTHON) >/dev/null || (echo "error: $(PYTHON) not found" && false)
	$(PYTHON) scripts/check_zed_requirements.py

# ── Build ──────────────────────────────────────────────────────

build: ## Compile WASM extension only (to stage extension.wasm use build-wasm)
	cargo build --target $(WASM_TARGET) --release
	@echo "WASM built: $(WASM_OUT)"
	@echo "Note: Zed's extension_builder encodes as component on 'Install Dev Extension'."
	@echo "      extension.wasm at repo root is not needed for dev and is gitignored."

build-wasm: ## Build WASM extension and copy it to extension.wasm (rustc emits a component for wasm32-wasip2)
	cargo build --target $(WASM_TARGET) --release
	@magic=$$(od -An -tx1 -j4 -N4 $(WASM_OUT) 2>/dev/null | tr -d ' \n'); \
	if [ "$$magic" = "0d000100" ]; then \
		cp $(WASM_OUT) extension.wasm; \
		echo "WASM component: extension.wasm ($$(stat -f%z extension.wasm 2>/dev/null || stat -c%s extension.wasm) bytes)"; \
	elif command -v wasm-tools >/dev/null 2>&1; then \
		echo "note: cargo output is a core module — encoding via wasm-tools"; \
		wasm-tools component embed --dummy $(WASM_OUT) -o /tmp/$(WASM_CRATE).core.wasm \
			&& wasm-tools component new /tmp/$(WASM_CRATE).core.wasm -o extension.wasm \
			&& echo "WASM component: extension.wasm ($$(stat -f%z extension.wasm 2>/dev/null || stat -c%s extension.wasm) bytes)" \
			|| { cp $(WASM_OUT) extension.wasm; echo "warning: wasm-tools encoding failed — staged raw output instead"; }; \
	else \
		cp $(WASM_OUT) extension.wasm; \
		echo "warning: $(WASM_OUT) is a core module and wasm-tools is not installed — staged raw module"; \
		echo "hint: cargo install wasm-tools (or use Rust >= 1.84, where wasm32-wasip2 emits a component directly)"; \
	fi

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
	cargo clippy -p rsc-ls -- -D warnings

audit: ## Audit dependencies (requires cargo-audit)
	@command -v cargo-audit >/dev/null 2>&1 || (echo "install: cargo install cargo-audit" && false)
	cargo audit

# ── Install / Bootstrap ────────────────────────────────────────

install-deps: ## Install system dependencies for detected platform (macOS/Homebrew, Fedora/dnf, Arch/pacman, Debian/apt)
	@if [ "$(SKIP_SYSTEM)" = "1" ]; then \
		echo "skip: system packages (SKIP_SYSTEM=1)"; \
		exit 0; \
	fi
	@if [ "$(IS_DARWIN)" = "Darwin" ]; then \
		echo "==> [macOS] installing system dependencies"; \
		command -v brew >/dev/null 2>&1 || { echo "error: Homebrew not found — install it from https://brew.sh" >&2; exit 1; }; \
		xcode-select -p >/dev/null 2>&1 || { echo "error: Xcode Command Line Tools missing — run 'xcode-select --install'" >&2; exit 1; }; \
		echo "brew install python node"; \
		brew install python node; \
	elif [ -n "$(DNF)" ]; then \
		echo "==> [Fedora/RHEL] installing system dependencies"; \
		$(SUDO) $(DNF) install -y gcc gcc-c++ make curl git ca-certificates openssl-devel pkgconf-pkg-config python3 python3-pip nodejs npm; \
	elif command -v pacman >/dev/null 2>&1; then \
		echo "==> [Arch] installing system dependencies"; \
		$(SUDO) pacman -Sy --needed --noconfirm base-devel curl git python python-pip nodejs npm; \
	elif [ -n "$(APT)" ]; then \
		echo "==> [Debian/Ubuntu] installing system dependencies"; \
		$(SUDO) apt-get update && $(SUDO) $(APT) install -y build-essential curl git ca-certificates pkg-config libssl-dev python3 python3-venv python3-pip nodejs npm; \
	else \
		echo "error: unsupported platform — see README 'Dependencies' for manual instructions" >&2; \
		exit 1; \
	fi

install-tools: ## Install language toolchains: rustup+wasm32-wasip2, Python venv (.venv) with pytest/requests/paramiko, tree-sitter-cli (npm, local)
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
		echo "hint: $(GRAMMAR_DIR) not present — skipping tree-sitter-cli setup (run 'make grammar-clone' first, then re-run install-tools)"; \
	elif command -v npm >/dev/null 2>&1; then \
		echo "==> Installing tree-sitter-cli (npm install --ignore-scripts in $(GRAMMAR_DIR))"; \
		cd $(GRAMMAR_DIR) && npm install --ignore-scripts && \
		{ [ -x node_modules/tree-sitter-cli/tree-sitter ] || \
		  (cd node_modules/tree-sitter-cli && node install.js); }; \
	else \
		echo "hint: npm not found — skipping tree-sitter-cli (grammar dev is optional; install Node.js or run 'npm install -g tree-sitter-cli')"; \
	fi
	@echo "==> Toolchains ready: cargo + $(WASM_TARGET), $(VENV_DIR)/bin/python (pip, pytest, requests, paramiko), tree-sitter-cli via npx"
	@echo "    Activate the venv in your shell: source $(VENV_DIR)/bin/activate"

install-lsp: build-lsp ## Build rsc-ls and copy to PATH (~/.cargo/bin; + /opt/homebrew/bin on macOS, ~/.local/bin on Linux)
	@mkdir -p ~/.cargo/bin
	cp target/release/rsc-ls ~/.cargo/bin/rsc-ls
	@chmod +x ~/.cargo/bin/rsc-ls
	@echo "Installed: ~/.cargo/bin/rsc-ls"
	@if [ -n "$(IS_DARWIN)" ] && [ -d /opt/homebrew/bin ]; then cp target/release/rsc-ls /opt/homebrew/bin/rsc-ls && echo "Installed: /opt/homebrew/bin/rsc-ls (for Zed GUI)"; fi
	@if [ -z "$(IS_DARWIN)" ] && [ -d "$$HOME/.local/bin" ]; then cp target/release/rsc-ls "$$HOME/.local/bin/rsc-ls" && echo "Installed: $$HOME/.local/bin/rsc-ls"; fi
	@echo "Verify: which rsc-ls && rsc-ls --help 2>&1 | head -n 5 || echo 'rsc-ls ready'"

install: install-deps install-tools install-lsp ## Full bootstrap: system deps + toolchains + build & install rsc-ls (idempotent; SKIP_SYSTEM=1 to skip distro packages)
	@echo "==> Bootstrap complete."

bump: ## Bump version (usage: make bump VERSION=0.2.0)
	@test -n "$(VERSION)" || (echo "usage: make bump VERSION=0.2.0" && false)
	@echo "$(VERSION)" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$$' || \
		(echo "error: VERSION must be semver x.y.z" && false)
	@echo "Bumping to $(VERSION) ..."
	@sed -i '' 's/^version = ".*"/version = "$(VERSION)"/' Cargo.toml lsp/Cargo.toml 2>/dev/null || sed -i 's/^version = ".*"/version = "$(VERSION)"/' Cargo.toml lsp/Cargo.toml
	@echo "Note: grammar versions live in the separate tree-sitter-rsc repo"
	@echo "      (untracked grammars/rsc working copy) — never bumped from here:"
	@echo "      publish_grammar.py would auto-commit them into that repo."
	@sed -i '' 's/^version = ".*"/version = "$(VERSION)"/' extension.toml 2>/dev/null || sed -i 's/^version = ".*"/version = "$(VERSION)"/' extension.toml
	@echo "Note: Cargo.lock refreshes on the next cargo command — commit it with the bumps."
	@echo "Bumped. Now run: cargo check && git diff"

# ── Cleanup ────────────────────────────────────────────────────

# Shared artifact removal core for clean / clean-generated (kept out of `make help`).
clean-artifacts:
	rm -rf target/
	rm -f extension.wasm
	rm -f grammars/rsc.wasm
	cd $(GRAMMAR_DIR) && rm -f parser.dylib tree-sitter-rsc.wasm

clean: clean-artifacts ## Remove build artifacts (preserves Cargo.lock and parser.c)
	cd $(GRAMMAR_DIR) && rm -rf target/ build/ node_modules/ 2>/dev/null || true

clean-generated: ## Remove ALL generated files including parser.c (use with care, asks confirmation first)
	@read -p "Remove parser.c, grammar.json, node-types.json? [y/N] " confirm && [ "$$confirm" = "y" ] || (echo "aborted" && exit 1)
	@$(MAKE) --no-print-directory clean-artifacts
	cd $(GRAMMAR_DIR) && rm -rf target/ build/ src/grammar.json src/node-types.json src/parser.c
	@echo "Note: parser.c removed — run 'make generate' to regenerate"

# ── Development ────────────────────────────────────────────────

install-dev: ## Point Zed to this directory (manual: Zed > Install Dev Extension)
	@echo "Open Zed → Command Palette → 'Install Dev Extension' → select this directory"
	@echo ""
	@echo "Make sure rsc-ls binary is in PATH:"
	@echo "  make build-lsp && make install-lsp"
	@echo "  # or: cargo build -p rsc-ls --release && export PATH=\"\$$PWD/target/release:\$$PATH\" && open -a Zed ."

validate: check-manifest generate-check fmt clippy test-all extract ## Full local gate (manifest, generate-check, fmt, clippy, tests, extract). Upstream-docs staleness is gated separately by sync-check in CI.
	@echo "All checks passed. Ready to commit."
