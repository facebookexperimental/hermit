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

.PHONY: build install-deps release-core help checkout-all check-submodules validate lint \
	validate-kvm validate-dbi validate-sabre validate-liteinst validate-e9patch

build: install-deps ## Build the development Hermit binary with every backend
	CARGO_BUILD_JOBS=$(THIRD_PARTY_BUILD_JOBS) $(CARGO) build --locked \
		-p hermit --features third-party-backends

install-deps: check-submodules ## Build and stage all third-party backend runtimes and plugins
	CARGO_BUILD_JOBS=$(THIRD_PARTY_BUILD_JOBS) $(CARGO) build --release --locked \
		-p detcore-dbi -p detcore-sabre -p hermit-install

release-core: check-submodules ## Build the lean core-only release binary (ptrace/kvm/liteinst)
	$(CARGO) build --release --locked -p hermit

# NOTE: `validate` MUST stay a .PHONY target with an explicit recipe. Without it,
# GNU Make's built-in implicit rule "%: %.sh" (cat $< >$@; chmod a+x $@) fires
# against validate.sh and merely COPIES it to a file named `validate` instead of
# running validation. .PHONY + this recipe overrides that implicit rule.
validate: check-submodules ## Run the full multi-backend validation suite (pass extra flags via ARGS="--help")
	./validate.sh $(ARGS)

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
lint: ## Run the full lint suite matching CI (rustfmt, shellcheck, whitespace, clippy, reverie pin)
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
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(SUBMODULE_PROXY) ./scripts/check-reverie-pin.rs

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

checkout-all: ## Initialize every pinned submodule before builds and validation
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
