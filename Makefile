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
	install-build-tools check-submodules verify-submodules check-skill-discovery validate validate-plan \
	validate-self-test validate-timeout-layers-test lint \
	validate-kvm validate-dbt validate-sabre validate-liteinst validate-e9patch

build: prune-stale-release install-deps ## Build the development Hermit binary with every backend
	@echo 'make: building the hermit binary (dev profile, third-party-backends) -- expect ~45s warm, longer cold'
	@echo "make: cargo jobs=$(THIRD_PARTY_BUILD_JOBS) (host has $$(nproc) logical cores)"
	CARGO_BUILD_JOBS=$(THIRD_PARTY_BUILD_JOBS) $(CARGO) build --locked \
		-p hermit --features third-party-backends
	@bin=target/debug/hermit; \
		if [ -x "$$bin" ]; then \
			echo "make: BUILD OK -- $$bin (dev profile, third-party-backends)"; \
		else \
			echo "make: BUILD INCOMPLETE -- cargo reported success but $$bin is absent" >&2; \
			exit 1; \
		fi

# `install-deps` is the "install everything this repo needs to build" entrypoint,
# so it opts into best-effort auto-installation of the native build toolchain via
# INSTALL_BUILD_TOOLS. Target-specific variables propagate to prerequisites, so
# the transitive `check-build-tools` prereq sees this and installs before it
# asserts. `validate`/`release-core` do NOT set it and therefore only assert.
install-deps: INSTALL_BUILD_TOOLS := 1
install-deps: install-hooks check-submodules ## Build and stage all third-party backend runtimes and plugins
	@echo 'make: building the third-party backend runtimes (release profile) -- expect ~60s cold'
	@echo "make: cargo jobs=$(THIRD_PARTY_BUILD_JOBS) (host has $$(nproc) logical cores)"
	CARGO_BUILD_JOBS=$(THIRD_PARTY_BUILD_JOBS) $(CARGO) build --release --locked \
		-p detcore-dbt -p detcore-sabre -p hermit-install
	@echo 'make: backend runtimes OK (release profile: detcore-dbt, detcore-sabre, hermit-install)'

# Install this clone's git pre-commit hooks (core.hooksPath -> .githooks) so a
# fresh clone/worktree gets the BLOCKING local pin-consistency check plus the
# non-blocking forward-advance advisory without a manual step. An ancestral,
# monotonic pin may remain behind the live Reverie tip. core.hooksPath is
# per-repo local config (not tracked), so it must be set once per checkout;
# wiring it into install-deps is that step.
install-hooks: ## Install this checkout's git pre-commit hooks (Reverie pin policy)
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
	if [ ! -x "$$bin" ]; then \
		echo "make: prune-stale-release: none present (no $$bin to prune)"; \
		exit 0; \
	fi; \
	head=$$(git rev-parse --short=12 HEAD 2>/dev/null || true); \
	ver=$$("$$bin" --version 2>/dev/null || true); \
	if [ -n "$$(git status --porcelain 2>/dev/null)" ]; then \
		reason="worktree has uncommitted changes"; \
	elif [ -n "$$head" ] && printf '%s' "$$ver" | grep -q "$$head" && ! printf '%s' "$$ver" | grep -q -- '-dirty'; then \
		echo "make: prune-stale-release: kept $$bin (built from current HEAD, worktree clean)"; \
		exit 0; \
	else \
		reason="built from '$$ver', HEAD is g$$head"; \
	fi; \
	rm -f "$$bin"; \
	echo "make: removed stale $$bin ($$reason); run 'make release-core' to rebuild it" >&2

# Keep `validate` as an explicit .PHONY convenience target for the sole Rust
# validation entrypoint.
validate: verify-submodules ## Run the full validation suite (Rust driver; pass flags via ARGS)
	./scripts/validate.rs $(ARGS)

validate-plan: ## Print the boxed DAG plan (nodes, wall/CPU/memory caps, deps) without running it
	./scripts/validate.rs --show-plan $(ARGS)

validate-self-test: ## Run the validate driver's inert policy/quoting/corpus brackets
	./scripts/validate.rs --self-test

validate-timeout-layers-test: ## Live bracket for step/scope timeouts (requires systemd --user + cgroup v2)
	./ci/validate-timeout-layers-test.sh

check-skill-discovery: ## Verify Claude and stock Codex discover the same product skills
	./scripts/check-skill-discovery.rs

# `make lint` mirrors the lint portion of canonical local validation, so a
# developer can reproduce every lint failure locally before pushing. Cheap checks run first for
# fast feedback; the compile-heavy clippy pass and the networked Reverie-pin
# ancestry/monotonicity policy run last. The exact clippy/rustfmt invocations match
# ci/dag/validate.json's portable label (lint.clippy / lint.rustfmt).
#
# shellcheck runs at --severity=error: an enforceable floor that is clean on
# current main (0/122 tracked scripts fail at error level) while 24 still carry
# warning/style findings. Ratchet the severity down (warning -> style) as that
# debt is retired rather than blocking the target on it today.
# SPLIT INTO TWO PREREQUISITES so that CI can schedule the checkers as one node.
# `make lint` is unchanged for humans. The split exists because a single DAG node
# running the whole target would re-run `cargo clippy` -- a measured 300s in
# ci/dag/validate.json's lint.clippy hint -- a second time per validate, and would
# run it OUTSIDE ci/run-with-reverie-dbt-budget.sh, which that node wraps it in.
# So lint-cargo holds exactly the two steps that already have byte-identical DAG
# nodes (lint.rustfmt, lint.clippy) and stays unscheduled here; lint-checks holds
# everything else and IS scheduled, as the single node check.lint_checks.
#
# ADD NEW CHECKERS TO lint-checks, NOT HERE. That is the whole point of the split:
# a checker added to lint-checks is exercised by local validation automatically, whereas the previous
# arrangement required someone to also hand-write a DAG node and six of the ten
# checkers in this target had no such node (measured 2026-08-25 at a5fef7ff7623).
lint: lint-checks lint-cargo ## Run the full lint suite matching CI (rustfmt, shellcheck, whitespace, clippy, Reverie pin policy, nested lockfiles, record-version floor)

# The full stop-path checker is safe to run inside validation: it clears the outer
# HERMIT_VALIDATE_ACTIVE marker before launching fixtures, and every full-validate
# child enters the stop-test seam before admission or the DAG. The full run also
# exercises the final-status contract, so it replaces the narrower self-test here.
#
# ⚠️ Comments INSIDE the recipe below must be TAB-indented. A comment at column 0 ends
# the recipe, silently dropping every line after it.
lint-checks: ## The lint checkers CI schedules as one node (everything in `lint` except the two cargo passes)
	./scripts/check-skill-discovery.rs
	./scripts/check-github-actions-triggers.rs
	./scripts/test-pre-push-submodule-diagnosis.sh
	./scripts/test-required-check-outcomes.sh
	./scripts/test-check-status-outcome.sh
	python3 ./scripts/test_check_outcome_adapter_authority.py
	./scripts/test-authority-obtained-once.sh
	bash ./tests/compat/real_compat_workload.sh --self-test-localhost-port
	python3 ./scripts/test_validate_stop_paths.py
	./scripts/check-merge-gate-policy.sh
	./scripts/test-configure-merge-gate-ruleset.sh
	python3 ./scripts/test_pr_status.py
	./scripts/run-script-tests.sh
	./scripts/bisect-probe.rs --self-test
	./ci/lint-checks-node.sh --self-test
	./ci/liteinst-strict-node.sh --self-test
	./ci/hermetic/assert-build-dependencies.sh --self-test
	./ci/hermetic/tests/test-retry-fetch.sh
	./scripts/check-checker-scheduling.rs --self-test
	./scripts/check-checker-scheduling.rs
	python3 ./scripts/check-validate-refusal-predicate.py --self-test
	python3 ./scripts/check-validate-refusal-predicate.py
	python3 ./ci/audit-test-binary-registration.py
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
	./ci/verify-submodules.sh --self-test
	./ci/verify-submodules.sh
	$(SUBMODULE_PROXY) ./ci/run-reverie-pin-check.sh
	$(SUBMODULE_PROXY) ./scripts/check-nested-lockfiles.rs
	./scripts/check-record-version-floor.rs
	./scripts/core-review-protocol-lint-test.sh
	python3 ./ci/test_audit_test_binary_registration.py
	./ci/run-with-reverie-dbt-budget-test.sh

lint-cargo: ## The two compile-heavy lint passes; CI runs these as lint.rustfmt and lint.clippy
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

help: ## Show this help (the list of make targets)
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?## / {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf '\nPer-backend validate targets run ONLY one backend'"'"'s compatibility\n'
	@printf 'corpus, for a tight per-backend iteration loop. Runtimes are approximate:\n'
	@printf '  validate-kvm       KVM parity corpus     (needs /dev/kvm)            ~5-15 min\n'
	@printf '  validate-dbt       DBT parity corpus     (third-party-backends)      ~5-15 min\n'
	@printf '  validate-sabre     SaBRe corpus          (needs HERMIT_SABRE_BINARY) ~10-20 min\n'
	@printf '  validate-liteinst  LiteInst strict corpus                            ~5-15 min\n'
	@printf '  validate-e9patch   e9patch corpus        (needs HERMIT_E9PATCH_BACKEND) ~5-20 min\n'
	@printf '\nThe full multi-backend suite is ./scripts/validate.rs (see ./scripts/validate.rs --help).\n'

# Detect the native build toolchain (cmake + a C and C++ compiler) that the
# third-party backends need. reverie-dbt's build.rs CMake-configures DynamoRIO
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
		echo "  The DBT backend builds DynamoRIO from source with CMake; without" >&2; \
		echo "  these the build fails ~30s in with a cryptic \"failed to configure" >&2; \
		echo "  DynamoRIO: No such file or directory\". Install them, for example:" >&2; \
		echo "    Debian/Ubuntu: sudo apt-get install -y cmake build-essential" >&2; \
		echo "    Fedora/RHEL:   sudo dnf install -y cmake gcc gcc-c++ make" >&2; \
		echo "  or run 'make install-deps', which installs them automatically." >&2; \
		exit 1; \
	fi; \
	echo "make: build tools OK -- cmake, a C compiler and a C++ compiler are all on PATH"

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
	@before="$$($(SUBMODULE_GIT) submodule status --recursive 2>/dev/null || true)"; \
		$(SUBMODULE_GIT) submodule update --init --recursive; \
		inited="$$(printf '%s\n' "$$before" | grep -E '^-' | awk '{print $$2}' | tr '\n' ' ')"; \
		repaired="$$(printf '%s\n' "$$before" | grep -E '^[+U]' | awk '{print $$2}' | tr '\n' ' ')"; \
		if [ -n "$$inited" ] || [ -n "$$repaired" ]; then \
			[ -n "$$inited" ] && echo "make: checkout-all: INITIALIZED (were absent): $$inited"; \
			if [ -n "$$repaired" ]; then \
				echo "make: checkout-all: REPAIRED DRIFT (were at a different revision): $$repaired"; \
				echo "  this is a SILENT CORRECTION on the build path -- run 'make verify-submodules'"; \
				echo "  to see drift REFUSED instead of repaired."; \
			fi; \
		else \
			echo "make: checkout-all: nothing to initialize or repair (all submodules already at their pins)"; \
		fi

verify-submodules: ## Verify submodule pins without initializing or repairing them
	@SUBMODULE_GIT='$(SUBMODULE_GIT)' ./ci/verify-submodules.sh

check-submodules: checkout-all ## Initialize if needed, then verify (build path)
	@$(MAKE) --no-print-directory verify-submodules

# ---------------------------------------------------------------------------
# Per-backend validation targets.
#
# Each target runs ONLY its backend's compatibility corpus so a backend lane
# agent can iterate tightly without paying for the full cross-backend suite.
# They wrap the pre-existing mechanisms rather than adding new ones:
#   * KVM and DBT (real Detcore backends) -> the backend-parity matrix,
#     scoped to one backend with `run_matrix.py --backend <backend>`, exactly
#     as the Rust validation driver's full backend-compatibility gate invokes it.
#   * SaBRe / LiteInst / e9patch          -> the Rust driver's focused
#     `--<backend>-compat-only` profiles, which self-build the release binary
#     and any backend artifacts.
# ---------------------------------------------------------------------------

validate-kvm: check-submodules ## Run ONLY the KVM backend parity corpus (needs /dev/kvm)
	cargo build -p hermit
	$(RUN_MATRIX) --hermit $(HERMIT_DEBUG_BIN) --backend kvm --probe-gaps --require-backend

validate-dbt: check-submodules ## Run ONLY the DBT backend parity corpus (third-party-backends feature)
	cargo build -p hermit --features third-party-backends
	$(RUN_MATRIX) --hermit $(HERMIT_DEBUG_BIN) --backend dbt --probe-gaps --require-backend

validate-sabre: check-submodules ## Run ONLY the SaBRe compatibility corpus (needs HERMIT_SABRE_BINARY)
	./scripts/validate.rs --sabre-compat-only

validate-liteinst: check-submodules ## Run ONLY the LiteInst strict compatibility corpus
	./scripts/validate.rs --liteinst-compat-only

validate-e9patch: check-submodules ## Run ONLY the e9patch (ptrace-preprocessing) compat corpus (needs HERMIT_E9PATCH_BACKEND)
	./scripts/validate.rs --e9patch-compat-only
