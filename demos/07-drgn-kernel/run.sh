#!/usr/bin/env bash
# Demo 7: reproducible Linux task evolution, observed with drgn from outside
# the guest, starting from demo 5's QEMU snapshot.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMOS_DIR="$(cd "$DEMO_DIR/.." && pwd)"
ROOT="$(cd "$DEMOS_DIR/.." && pwd)"
# shellcheck source=demos/lib/qemu-paths.sh
source "$DEMOS_DIR/lib/qemu-paths.sh"
ASSETS="${QEMU_ASSETS:-$(qemu_default_assets "$ROOT")}"
ARTIFACTS="${DEMO07_ARTIFACTS:-$ROOT/target/demos/07-drgn-kernel}"

usage() {
  cat <<'EOF'
Usage: demos/07-drgn-kernel/run.sh

Restore demo 5's QEMU/Linux snapshot twice. Each restore takes a read-only drgn
snapshot of the kernel task list, lets the guest run a fixed command that starts
two tasks and requests a 1000-microsecond sleep, pauses the guest when the
command prints its completion marker, and takes a second task-list snapshot.
The sleep is what the command asks for; how far guest time moves is not
measured. The two restores must produce the same before, after, and
difference. The drgn reads execute no guest instructions.

Useful overrides:
  DEMO07_RUNS=2              independent restores (minimum and default: 2)
  DEMO07_TASK_LIMIT=16       displayed task-list prefix (all rows are compared)
  QEMU_BIN=/path             qemu-system-x86_64 binary
  QEMU_ASSETS=/path          bzImage/initramfs cache
  DEMO07_SNAPSHOT_DISK=/path demo 5 boot snapshot, used only with demo 5's matching
                             record (the default one is rebuilt by demo 5)
  DEMO07_SNAPSHOT_NAME=name  internal snapshot name (default: hermit-boot)
  DEMO07_VMLINUX=/path       matching ELF debug/BTF image (extracted from bzImage by default)
  DEMO07_TIMEOUT=240         restore/advance timeout in seconds
  DEMO07_ARTIFACTS=/path     per-pass run directories (default: target/demos/07-drgn-kernel)
  DEMO07_QEMU_BIOS=/path     QEMU firmware directory, for a QEMU in a non-standard prefix
  DEMO07_QEMU_LIBRARY_PATH=/path  extra LD_LIBRARY_PATH for that QEMU
EOF
}

case "${1:-}" in
  "") ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

if ! command -v hermit >/dev/null 2>&1; then
  echo "error: hermit is not on PATH -- build it with 'make -C $ROOT release-core' and run: export PATH=\"$ROOT/target/release:\$PATH\"" >&2
  exit 1
fi
DRGN_BIN="${DRGN_BIN:-$(command -v drgn || true)}"
if [ -z "$DRGN_BIN" ]; then
  echo "error: drgn is required (https://github.com/osandov/drgn)" >&2
  exit 1
fi
for tool in bpftool gcc readelf; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool is required to give drgn the kernel's type information" >&2
    exit 1
  fi
done

QEMU_BIN="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
if [ -z "$QEMU_BIN" ] && [ -x /usr/libexec/qemu-kvm ]; then
  QEMU_BIN=/usr/libexec/qemu-kvm
fi
if [ -z "$QEMU_BIN" ] || [ ! -x "$QEMU_BIN" ]; then
  echo "error: qemu-system-x86_64 is required (or set QEMU_BIN)" >&2
  exit 1
fi

DEMO07_KERNEL="${DEMO07_KERNEL:-$ASSETS/bzImage}"
DEMO07_INITRD="${DEMO07_INITRD:-$ASSETS/initramfs.cpio.gz}"
DEMO07_SNAPSHOT_DISK="${DEMO07_SNAPSHOT_DISK:-$ASSETS/hermit-boot.qcow2}"
if [ ! -r "$DEMO07_KERNEL" ] || [ ! -r "$DEMO07_INITRD" ]; then
  QEMU_ASSETS="$ASSETS" QEMU_BIN="$QEMU_BIN" "$DEMOS_DIR/lib/qemu-assets.sh"
fi
if [ ! -r "$DEMO07_KERNEL" ] || [ ! -r "$DEMO07_INITRD" ]; then
  echo "error: QEMU kernel/initramfs provisioning failed under $ASSETS" >&2
  exit 1
fi
# The boot snapshot is not checked here: that it exists says nothing about
# the /init in its memory. task_evolution.py restores it only when demo 5's
# record of it (<snapshot>.producer.json) names it and the initramfs in
# $ASSETS now, with demo 6's rules: it runs demo 5 to build or rebuild the
# default snapshot, and refuses a DEMO07_SNAPSHOT_DISK without such a record,
# saying how to rebuild it. Each pass checks its copy of the snapshot again.

DEMO07_VMLINUX="${DEMO07_VMLINUX:-$ASSETS/vmlinux}"
mkdir -p "$ARTIFACTS"

export QEMU_BIN DEMO07_KERNEL DEMO07_INITRD DEMO07_VMLINUX
export DEMO07_SNAPSHOT_DISK
export DEMO07_ASSETS="$ASSETS"
export DEMO07_SNAPSHOT_NAME="${DEMO07_SNAPSHOT_NAME:-hermit-boot}"
export DEMO07_ARTIFACTS="$ARTIFACTS"
export DEMO07_QEMU_BIOS="${DEMO07_QEMU_BIOS:-}"
export DEMO07_QEMU_LIBRARY_PATH="${DEMO07_QEMU_LIBRARY_PATH:-}"

echo "=== Demo 7: snapshot -> read-only kernel read -> deterministic advance -> read ==="
echo "Using $(hermit --version 2>/dev/null || echo 'hermit (version unavailable)') ($(command -v hermit))"
# With no target, drgn opens the running host kernel, which needs root. Give it
# this shell as a harmless target instead; task_evolution.py builds its own drgn
# program for the guest kernel.
"$DRGN_BIN" -q -p "$$" "$DEMO_DIR/task_evolution.py"
echo
echo "=== Demo 7: drgn Kernel Task Evolution: SUCCESS ==="
