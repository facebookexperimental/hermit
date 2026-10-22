# SPDX-License-Identifier: GPL-2.0-only
#
# Builds hermit_cpuid_fault.ko against the running kernel (or KDIR=...), and
# the cpuid-fault-probe helper. enable-cpuid-faulting.sh drives this; see
# README.md.
#
# A kernel built by clang (CONFIG_CC_IS_CLANG=y in its config, or "clang
# version" in /proc/version for the running kernel) must have its modules built
# with the same LLVM toolchain: gcc rejects clang-only flags such as
# -fsplit-lto-unit. LLVM=1 is then passed to kbuild unless LLVM is already set.

ifneq ($(KERNELRELEASE),)
# Included by kbuild.
obj-m := hermit_cpuid_fault.o
else

KDIR ?= /lib/modules/$(shell uname -r)/build
CC_PROBE ?= cc

KERNEL_IS_CLANG := $(shell grep -qs '^CONFIG_CC_IS_CLANG=y' $(KDIR)/.config && echo 1)
ifeq ($(KERNEL_IS_CLANG),)
ifeq ($(realpath $(KDIR)),$(realpath /lib/modules/$(shell uname -r)/build))
KERNEL_IS_CLANG := $(shell grep -qs 'clang version' /proc/version && echo 1)
endif
endif
LLVM ?= $(KERNEL_IS_CLANG)

.PHONY: all module probe clean print-llvm

all: module probe

module:
	$(MAKE) -C $(KDIR) M=$(CURDIR) LLVM=$(LLVM) modules

# Bind every symbol at startup, so nothing runs the dynamic linker (which may
# execute cpuid) after the probe arms CPUID faulting.
probe: cpuid-fault-probe

cpuid-fault-probe: cpuid-fault-probe.c
	$(CC_PROBE) -O2 -Wall -Wextra -Werror -Wl,-z,now -o $@ $<

print-llvm:
	@echo "LLVM=$(LLVM)"

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) LLVM=$(LLVM) clean
	rm -f cpuid-fault-probe

endif
