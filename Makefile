SUBMODULE_PROXY ?= $(shell command -v with-proxy 2>/dev/null)
SUBMODULE_GIT = $(SUBMODULE_PROXY) git
CARGO_PROXY ?= $(SUBMODULE_PROXY)
CARGO = $(CARGO_PROXY) cargo
# Keep Cargo and nested native builds wide enough for high-core CI hosts without
# immediately saturating every hardware thread. Override on smaller shared hosts.
THIRD_PARTY_BUILD_JOBS ?= 64

# Hermit debug binary used by the per-backend parity targets below. Override to
# point the matrix at a prebuilt binary and skip the build step, e.g.
#   make validate-kvm HERMIT_DEBUG_BIN=/path/to/hermit
HERMIT_DEBUG_BIN ?= target/debug/hermit
RUN_MATRIX = python3 tests/backend-parity/run_matrix.py

.DEFAULT_GOAL := build

.PHONY: build install-deps install-hooks release-core prune-stale-release help checkout-all check-build-tools \
	install-build-tools check-submodules check-skill-discovery validate lint \
	validate-kvm validate-dbi validate-sabre validate-liteinst validate-e9patch

build: prune-stale-release install-deps ## Build the development Hermit binary with every backend
	CARGO_BUILD_JOBS=$(THIRD_PARTY_BUILD_JOBS) $(CARGO) build --locked \
		-p hermit --features third-party-backends

# `install-deps` is the "install everything this repo needs to build" entrypoint,
# so it opts into best-effort auto-installation of the native build toolchain via
# INSTALL_BUILD_TOOLS. Target-specific variables propagate to prerequisites, so
# the transitive `check-build-tools` prereq sees this and installs before it
# asserts. `validate`/`release-core` do NOT set it and therefore only assert.
install-deps: INSTALL_BUILD_TOOLS := 1
install-deps: install-hooks check-submodules ## Build and stage all third-party backend runtimes and plugins
	CARGO_BUILD_JOBS=$(THIRD_PARTY_BUILD_JOBS) $(CARGO) build --release --locked \
		-p detcore-dbi -p detcore-sabre -p hermit-install

# Install this clone's git pre-commit hooks (core.hooksPath -> .githooks) so a
# fresh clone/worktree gets the BLOCKING Reverie pin-drift gate without a manual
# step. core.hooksPath is per-repo local config (not tracked), so it must be set
# once per checkout; wiring it into install-deps is that step.
install-hooks: ## Install this checkout's git pre-commit hooks (Reverie pin gate)
	@./scripts/setup-hooks.sh

release-core: check-submodules ## Build the lean core-only release binary (ptrace/kvm/liteinst)
	$(CARGO) build --release --locked -p hermit

# `make build` produces target/debug/hermit but never rebuilds an existing
# target/release/hermit. A release binary left over from an earlier
# `make release-core` (or from a different commit) is then STALE: the documented
# release smoke commands (README, docs/QEMU_BOOT.md, docs/SABRE_COMPATIBILITY.md)
# run `./target/release/hermit`, which exits 0 while silently testing old code.
# To make that impossible, `build` depends on this target, which REMOVES a stale
# release binary (rebuild-or-remove: rebuild explicitly with `make release-core`).
# "Current" means the embedded --version SHA equals HEAD's `git rev-parse
# --short=12` on a clean worktree with no `-dirty` marker, matching how
# hermit-cli/build.rs stamps the binary. A dirty worktree can't be verified, so
# any existing release binary is treated as stale and removed.
prune-stale-release: ## Remove target/release/hermit if stale (not built from current HEAD/worktree)
	@bin=target/release/hermit; \
	[ -x "$$bin" ] || exit 0; \
	head=$$(git rev-parse --short=12 HEAD 2>/dev/null || true); \
	ver=$$("$$bin" --version 2>/dev/null || true); \
	if [ -n "$$(git status --porcelain 2>/dev/null)" ]; then \
		reason="worktree has uncommitted changes"; \
	elif [ -n "$$head" ] && printf '%s' "$$ver" | grep -q "$$head" && ! printf '%s' "$$ver" | grep -q -- '-dirty'; then \
		exit 0; \
	else \
		reason="built from '$$ver', HEAD is g$$head"; \
	fi; \
	rm -f "$$bin"; \
	echo "make: removed stale $$bin ($$reason); run 'make release-core' to rebuild it" >&2

# NOTE: `validate` MUST stay a .PHONY target with an explicit recipe. Without it,
# GNU Make's built-in implicit rule "%: %.sh" (cat $< >$@; chmod a+x $@) fires
# against validate.sh and merely COPIES it to a file named `validate` instead of
# running validation. .PHONY + this recipe overrides that implicit rule.
validate: check-submodules ## Run the full multi-backend validation suite (pass extra flags via ARGS="--help")
	./validate.sh $(ARGS)

check-skill-discovery: ## Verify Claude and stock Codex discover the same product skills
	./scripts/check-skill-discovery.rs

# `make lint` mirrors the lint gate CI's merge-gate enforces, so a developer can
# reproduce every lint failure locally before pushing. Cheap checks run first for
# fast feedback; the compile-heavy clippy pass and the networked Reverie-pin
# invariant run last. The exact clippy/rustfmt invocations match
# ci/dag/portable.json (lint.clippy / lint.rustfmt).
#
# shellcheck runs at --severity=error: an enforceable floor that is clean on
# current main (0/122 tracked scripts fail at error level) while 24 still carry
# warning/style findings. Ratchet the severity down (warning -> style) as that
# debt is retired rather than blocking the target on it today.
lint: ## Run the full lint suite matching CI (rustfmt, shellcheck, whitespace, clippy, reverie pin, nested lockfiles)
	./scripts/check-skill-discovery.rs
	./scripts/test-required-check-outcomes.sh
	./scripts/test-check-status-outcome.sh
	./scripts/check-merge-gate-policy.sh
	python3 ./scripts/test_pr_status.py
	$(CARGO) fmt --all -- --check
	@sh_files="$$(git ls-files '*.sh' ':!:third-party/**')"; \
		if [ -z "$$sh_files" ]; then \
			echo 'lint: no tracked shell scripts to check'; \
		elif command -v shellcheck >/dev/null 2>&1; then \
			printf '%s\n' "$$sh_files" | xargs shellcheck --severity=error; \
		else \
			echo 'error: shellcheck is not installed (see https://www.shellcheck.net)' >&2; \
			exit 1; \
		fi
	@git diff --check
	python3 scripts/test_validate_stop_paths.py
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(SUBMODULE_PROXY) ./scripts/check-reverie-pin.rs
	$(SUBMODULE_PROXY) ./scripts/check-nested-lockfiles.rs

help: ## Show this help (the list of make targets)
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?## / {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf '\nPer-backend validate targets run ONLY one backend'"'"'s compatibility\n'
	@printf 'corpus, for a tight per-backend iteration loop. Runtimes are approximate:\n'
	@printf '  validate-kvm       KVM parity corpus     (needs /dev/kvm)            ~5-15 min\n'
	@printf '  validate-dbi       DBI parity corpus     (third-party-backends)      ~5-15 min\n'
	@printf '  validate-sabre     SaBRe corpus          (needs HERMIT_SABRE_BINARY) ~10-20 min\n'
	@printf '  validate-liteinst  LiteInst strict corpus                            ~5-15 min\n'
	@printf '  validate-e9patch   e9patch corpus        (needs HERMIT_E9PATCH_BACKEND) ~5-20 min\n'
	@printf '\nThe full multi-backend suite is ./validate.sh (see ./validate.sh --help).\n'

# Detect the native build toolchain (cmake + a C and C++ compiler) that the
# third-party backends need. reverie-dbi's build.rs CMake-configures DynamoRIO
# roughly 30s into `cargo build`; when cmake or a compiler is absent that step
# fails to SPAWN cmake and panics with the cryptic "failed to configure
# DynamoRIO: No such file or directory (os error 2)" — an ENOENT on the cmake
# executable, not a missing source tree (a missing source instead makes cmake
# exit non-zero). `make install-deps` sets INSTALL_BUILD_TOOLS=1 to best-effort
# install the toolchain first; every other entrypoint asserts only and fails
# fast with an actionable message. On a warm box the tools are present and this
# is a no-op.
check-build-tools: ## Verify the native build toolchain (cmake + C/C++ compiler) is present
	@detect() { \
		miss=; \
		command -v cmake >/dev/null 2>&1 || miss="$$miss cmake"; \
		{ command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 \
			|| command -v clang >/dev/null 2>&1; } \
			|| miss="$$miss C-compiler(cc/gcc/clang)"; \
		{ command -v c++ >/dev/null 2>&1 || command -v g++ >/dev/null 2>&1 \
			|| command -v clang++ >/dev/null 2>&1; } \
			|| miss="$$miss C++-compiler(c++/g++/clang++)"; \
		printf '%s' "$$miss"; \
	}; \
	missing="$$(detect)"; \
	if [ -n "$$missing" ] && [ "$(INSTALL_BUILD_TOOLS)" = 1 ]; then \
		$(MAKE) --no-print-directory install-build-tools; \
		missing="$$(detect)"; \
	fi; \
	if [ -n "$$missing" ]; then \
		echo "error: required native build tool(s) not found on PATH:$$missing" >&2; \
		echo "  The DBI backend builds DynamoRIO from source with CMake; without" >&2; \
		echo "  these the build fails ~30s in with a cryptic \"failed to configure" >&2; \
		echo "  DynamoRIO: No such file or directory\". Install them, for example:" >&2; \
		echo "    Debian/Ubuntu: sudo apt-get install -y cmake build-essential" >&2; \
		echo "    Fedora/RHEL:   sudo dnf install -y cmake gcc gcc-c++ make" >&2; \
		echo "  or run 'make install-deps', which installs them automatically." >&2; \
		exit 1; \
	fi

# Best-effort install of the native build toolchain via the platform package
# manager. Invoked only from the `install-deps` path (INSTALL_BUILD_TOOLS=1).
# Uses non-interactive sudo so it never hangs on a password prompt in an
# automated context; if privileges or a package manager are unavailable it warns
# and returns success, leaving check-build-tools to emit the actionable error.
install-build-tools: ## Best-effort install of cmake + a C/C++ toolchain via the platform package manager
	@echo "install-deps: ensuring native build toolchain (cmake + C/C++ compiler) is installed..."; \
		SUDO=; \
		if [ "$$(id -u)" != 0 ]; then \
			if command -v sudo >/dev/null 2>&1; then SUDO="sudo -n"; \
			else echo "warning: not root and sudo unavailable; cannot auto-install build tools" >&2; exit 0; fi; \
		fi; \
		if command -v apt-get >/dev/null 2>&1; then \
			$$SUDO apt-get update && $$SUDO apt-get install -y cmake build-essential \
				|| echo "warning: apt-get could not install build tools (insufficient privileges?)" >&2; \
		elif command -v dnf >/dev/null 2>&1; then \
			$$SUDO dnf install -y cmake gcc gcc-c++ make \
				|| echo "warning: dnf could not install build tools (insufficient privileges?)" >&2; \
		elif command -v yum >/dev/null 2>&1; then \
			$$SUDO yum install -y cmake gcc gcc-c++ make \
				|| echo "warning: yum could not install build tools (insufficient privileges?)" >&2; \
		else \
			echo "warning: no supported package manager (apt-get/dnf/yum) found; install cmake + a C/C++ compiler manually" >&2; \
		fi

checkout-all: check-build-tools ## Initialize every pinned submodule before builds and validation
	@$(SUBMODULE_GIT) submodule update --init --recursive

check-submodules: checkout-all ## Verify every pinned submodule is checked out at its recorded revision
	@status="$$($(SUBMODULE_GIT) submodule status --recursive)"; \
		printf '%s\n' "$$status"; \
		if printf '%s\n' "$$status" | grep -Eq '^[-+U]'; then \
			echo 'error: a required submodule is missing or not at its pinned revision' >&2; \
			exit 1; \
		fi
	@test -f agent-utils/README.md || { echo 'error: agent-utils submodule is missing' >&2; exit 1; }
	@test -f third-party/rr/CMakeLists.txt || { echo 'error: rr submodule is missing' >&2; exit 1; }

# ---------------------------------------------------------------------------
# Per-backend validation targets.
#
# Each target runs ONLY its backend's compatibility corpus so a backend lane
# agent can iterate tightly without paying for the full cross-backend suite.
# They wrap the pre-existing mechanisms rather than adding new ones:
#   * KVM and DBI (real Detcore backends) -> the backend-parity matrix,
#     scoped to one backend with `run_matrix.py --backend <backend>`, exactly
#     as validate.sh's full "Real backend compatibility matrix" gate invokes it.
#   * SaBRe / LiteInst / e9patch          -> validate.sh's focused
#     `--<backend>-compat-only` profiles, which self-build the release binary
#     and any backend artifacts.
# ---------------------------------------------------------------------------

validate-kvm: check-submodules ## Run ONLY the KVM backend parity corpus (needs /dev/kvm)
	cargo build -p hermit
	$(RUN_MATRIX) --hermit $(HERMIT_DEBUG_BIN) --backend kvm --probe-gaps --require-backend

validate-dbi: check-submodules ## Run ONLY the DBI backend parity corpus (third-party-backends feature)
	cargo build -p hermit --features third-party-backends
	$(RUN_MATRIX) --hermit $(HERMIT_DEBUG_BIN) --backend dbi --probe-gaps --require-backend

validate-sabre: check-submodules ## Run ONLY the SaBRe compatibility corpus (needs HERMIT_SABRE_BINARY)
	./validate.sh --sabre-compat-only

validate-liteinst: check-submodules ## Run ONLY the LiteInst strict compatibility corpus
	./validate.sh --liteinst-compat-only

validate-e9patch: check-submodules ## Run ONLY the e9patch (ptrace-preprocessing) compat corpus (needs HERMIT_E9PATCH_BACKEND)
	./validate.sh --e9patch-compat-only
