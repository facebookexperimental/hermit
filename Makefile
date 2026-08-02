SUBMODULE_PROXY ?= $(shell command -v with-proxy 2>/dev/null)
SUBMODULE_GIT = $(SUBMODULE_PROXY) git

# Hermit debug binary used by the per-backend parity targets below. Override to
# point the matrix at a prebuilt binary and skip the build step, e.g.
#   make validate-kvm HERMIT_DEBUG_BIN=/path/to/hermit
HERMIT_DEBUG_BIN ?= target/debug/hermit
RUN_MATRIX = python3 tests/backend-parity/run_matrix.py

.DEFAULT_GOAL := help

.PHONY: help checkout-all \
	validate-kvm validate-dbi validate-sabre validate-liteinst validate-e9patch

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

checkout-all: ## Initialize/update all git submodules (uses with-proxy if present)
	@$(SUBMODULE_GIT) submodule update --init --recursive

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

validate-kvm: ## Run ONLY the KVM backend parity corpus (needs /dev/kvm)
	cargo build -p hermit
	$(RUN_MATRIX) --hermit $(HERMIT_DEBUG_BIN) --backend kvm --probe-gaps --require-backend

validate-dbi: ## Run ONLY the DBI backend parity corpus (third-party-backends feature)
	cargo build -p hermit --features third-party-backends
	$(RUN_MATRIX) --hermit $(HERMIT_DEBUG_BIN) --backend dbi --probe-gaps --require-backend

validate-sabre: ## Run ONLY the SaBRe compatibility corpus (needs HERMIT_SABRE_BINARY)
	./validate.sh --sabre-compat-only

validate-liteinst: ## Run ONLY the LiteInst strict compatibility corpus
	./validate.sh --liteinst-compat-only

validate-e9patch: ## Run ONLY the e9patch (ptrace-preprocessing) compat corpus (needs HERMIT_E9PATCH_BACKEND)
	./validate.sh --e9patch-compat-only
