# Targets for the Hermit demo suite. Every demo runs the `hermit` found on
# PATH; see README.md for how to build it.
#
# Demos 6 and 7 restore the Linux boot snapshot that demo 5 saves. The snapshot
# is a real file target: when it exists they restore it without rebooting, and
# when it is missing Make runs demo 5 once to produce it. Demo 6 also runs demo 5
# again itself when the snapshot was booted from another initramfs than the one
# lib/qemu-assets.sh builds now (demo 5 records that initramfs next to it).

DEMO_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
ROOT := $(abspath $(DEMO_DIR)..)
CLEAN := $(DEMO_DIR)clean.sh
PYTHON ?= python3
QEMU_ASSETS ?= $(shell "$(DEMO_DIR)lib/qemu-paths.sh" "$(ROOT)")
BOOT_SNAPSHOT := $(QEMU_ASSETS)/hermit-boot.qcow2
DEMO6_COMMAND ?= uname -a
DEMO_TARGETS := demo1 demo2 demo3 demo4 demo5 demo6 demo7 demo8 demo9

.PHONY: $(DEMO_TARGETS) all group1 group2 group3 clean distclean help \
	hermit-prereqs qemu-prereqs drgn-prereqs test

help:
	@echo "make demo1 .. demo4  Run one process-level demo."
	@echo "make demo5           Boot Linux in QEMU under Hermit and save a snapshot."
	@echo "make demo6           Restore the snapshot and run DEMO6_COMMAND='uname -a'."
	@echo "make demo7           Restore the snapshot and inspect the kernel with drgn."
	@echo "make demo8           Reproduce the btrfs-convert use-after-free (skips without assets)."
	@echo "make demo9           Boot a BusyBox userspace in QEMU under hermit run --strict."
	@echo "make all             Run demos 1-9 and print a PASS/FAIL/SKIP summary."
	@echo "make group1 .. group3  Run one verification group (see README.md)."
	@echo "make clean           Remove computed results, including the snapshot."
	@echo "make distclean       Also remove the downloaded kernel and built initramfs."
	@echo "make test            Run the fast unit tests for the demo scripts (no QEMU)."

hermit-prereqs:
	@command -v hermit >/dev/null 2>&1 || { \
		echo "error: hermit is not on PATH -- build it with 'make -C $(ROOT) release-core' and run: export PATH=\"$(ROOT)/target/release:\$$PATH\"" >&2; \
		exit 1; }

qemu-prereqs: hermit-prereqs
	@QEMU_ASSETS="$(QEMU_ASSETS)" "$(DEMO_DIR)lib/qemu-assets.sh" --check

drgn-prereqs: qemu-prereqs
	@for tool in drgn bpftool gcc readelf; do \
		command -v $$tool >/dev/null 2>&1 || { \
			echo "error: $$tool is required for demo 7." >&2; exit 1; }; \
	done

demo1: | hermit-prereqs
	@"$(DEMO_DIR)01-deterministic-run/run.sh"

demo2: | hermit-prereqs
	@"$(DEMO_DIR)02-record-replay/run.sh"

demo3: | hermit-prereqs
	@"$(DEMO_DIR)03-chaos-concurrency/run.sh"

demo4: | hermit-prereqs
	@"$(DEMO_DIR)04-schedule-bisection/run.sh"

# Demo 5 writes the snapshot atomically. The check afterwards catches a
# misconfigured QEMU_ASSETS or an incomplete boot.
$(BOOT_SNAPSHOT): | qemu-prereqs
	@echo "Boot snapshot missing; running demo 5 first..."
	@QEMU_ASSETS="$(QEMU_ASSETS)" "$(PYTHON)" "$(DEMO_DIR)05-qemu-boot/run.py"
	@test -r "$@" || { echo "error: demo 5 did not produce $@" >&2; exit 1; }

demo5: | qemu-prereqs
	@QEMU_ASSETS="$(QEMU_ASSETS)" "$(PYTHON)" "$(DEMO_DIR)05-qemu-boot/run.py"

demo6: $(BOOT_SNAPSHOT) | qemu-prereqs
	@QEMU_ASSETS="$(QEMU_ASSETS)" \
		QEMU_BOOT_SNAPSHOT_DISK="$(BOOT_SNAPSHOT)" \
		"$(PYTHON)" "$(DEMO_DIR)06-qemu-resume/run.py" $(DEMO6_COMMAND)

demo7: $(BOOT_SNAPSHOT) | drgn-prereqs
	@QEMU_ASSETS="$(QEMU_ASSETS)" \
		DEMO07_SNAPSHOT_DISK="$(BOOT_SNAPSHOT)" \
		"$(DEMO_DIR)07-drgn-kernel/run.sh"

# Demo 8 needs assets built by 08-btrfs-convert-uaf/prepare-assets.sh. Without
# them it prints SKIPPED and exits 77, so `make demo8` fails (make exits 2)
# rather than reporting a pass. run-all.sh sets DEMO_SKIP_EXIT_STATUS=0 and
# reports the demo as a skip from its SKIPPED line. A sweep with a skip exits 3,
# so `make all` and `make group2` also fail (make exits 2) until those assets
# exist.
demo8: | hermit-prereqs
	@"$(DEMO_DIR)08-btrfs-convert-uaf/run.sh"

demo9: | hermit-prereqs
	@"$(DEMO_DIR)09-qemu-busybox/run.sh"

all:
	@"$(DEMO_DIR)run-all.sh" --all

group1 group2 group3:
	@"$(DEMO_DIR)run-all.sh" --group $(patsubst group%,%,$@)

test:
	@$(PYTHON) -m unittest discover -s "$(DEMO_DIR)tests" -p 'test_*.py' -v

clean:
	@"$(CLEAN)"

distclean:
	@"$(CLEAN)" --distclean
