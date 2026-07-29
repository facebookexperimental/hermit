SUBMODULE_PROXY ?= $(shell command -v with-proxy 2>/dev/null)
SUBMODULE_GIT = $(SUBMODULE_PROXY) git

.PHONY: checkout-all

checkout-all:
	@$(SUBMODULE_GIT) submodule update --init --recursive
