#!/usr/bin/env bash
# Remove computed demo results so the next run starts fresh.
#
# Demos 5 and 6 save the result of their first run (demo 5: boot-anchor/;
# demo 6: resume-metadata/) and compare every later run against it. After
# the Hermit binary, the kernel, or a demo changes, that saved result no
# longer applies, and later runs report PARTIAL. Clean it away with:
#
#   ./clean.sh              remove computed results: saved first runs, run
#                           history, snapshots, logs, and runtime files.
#                           Keeps the downloaded kernel and built initramfs.
#   ./clean.sh --distclean  also remove the downloaded kernel and the built
#                           initramfs (bzImage, initramfs.cpio.gz, vmlinux).
#   ./clean.sh --dry-run    print what would be removed without deleting.
#
# The QEMU asset directory defaults to <repo>/ignored/qemu-linux, or to a
# checkout-specific /var/tmp directory when the checkout is under /tmp, and
# honors the same QEMU_ASSETS override the demos use. Only paths the demos
# create are removed.
#
# A results directory moved with DEMO07_ARTIFACTS, DEMO08_ARTIFACTS or
# DEMO_SWEEP_LOG_DIR is not removed: such a path can name any directory, and
# nothing records which entries in it a demo created. clean.sh prints its
# name instead, so that you can remove it yourself.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DEMO_DIR/.." && pwd)"
# shellcheck source=lib/qemu-paths.sh
source "$DEMO_DIR/lib/qemu-paths.sh"
ASSETS="${QEMU_ASSETS:-$(qemu_default_assets "$ROOT")}"

distclean=0
dry_run=0
for arg in "$@"; do
  case "$arg" in
    --distclean) distclean=1 ;;
    --dry-run|-n) dry_run=1 ;;
    -h|--help)
      sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      printf 'clean.sh: unknown argument: %s\n' "$arg" >&2
      printf "Try './clean.sh --help'.\n" >&2
      exit 2
      ;;
  esac
done

# Refuse a mis-resolved asset directory before deleting anything.
if [ -z "$ASSETS" ] || [ "$ASSETS" = "/" ]; then
  printf 'clean.sh: refusing to operate on unsafe QEMU_ASSETS=%q\n' "$ASSETS" >&2
  exit 1
fi

# Computed results under the QEMU asset directory; the next run recreates them.
computed=(
  "boot-anchor"                # demo 5's saved first run (directory)
  "boot-anchor.claim.lock"     # lock used while saving it
  ".work"                      # per-run working directories
  "run-metadata.json"          # older single-file form of the saved first run
  "run-history"                # demo 5's per-run archives
  "resume-metadata"            # demo 6's saved first runs and history
  "hermit-snapshot.qcow2"      # working snapshot disk
  "hermit-snapshot.qcow2.id"
  "hermit-snapshot.qcow2.producer.json"
  "hermit-boot.qcow2"          # demo 5's boot snapshot, restored by demos 6 and 7
  "hermit-boot.qcow2.producer.json" # the initramfs it was booted from
  "guest-command.img"          # demo 6's command disk
  "serial.log"                 # serial console capture
  "qmp.sock"                   # QEMU control socket
  ".qemu-demo.lock"            # demo 6's single-run lock
)

# Downloaded or built inputs; removed only by --distclean.
provisioned=(
  "bzImage"                    # the pinned kernel
  "vmlinux"                    # demo 7's kernel image with type information
  "initramfs.cpio.gz"          # BusyBox initramfs built by lib/qemu-assets.sh
  ".initramfs-version"         # initramfs cache version
  ".initramfs-build"           # the version and SHA-256 of the initramfs built
)

# Temporary files an interrupted run may leave behind. Each pattern is quoted
# whole, so it stays a pattern here and is expanded only under the asset
# directory below. With the `*` outside the quotes, it would expand here,
# against the directory clean.sh was started from, and the names it matched
# there would be removed instead.
transient_globs=(
  "run-metadata.json.tmp.*"
  "hermit-boot.qcow2.tmp.*"
  "hermit-boot.qcow2.producer.json.tmp.*"
  "hermit-snapshot.qcow2.producer.json.tmp.*"
  ".bzImage.*"
  ".initramfs.cpio.gz.*"
  ".initramfs-version.*"
  ".initramfs-build.*"
  ".vmlinux.*"
  ".vmlinux-types.*"
)

# Result directories under target/ for demos that do not use the asset
# directory. Demo 9's directory also caches its kernel, so it goes only with
# --distclean.
target_results=(
  "$ROOT/target/demos/07-drgn-kernel"
  "$ROOT/target/demos/08-btrfs-convert-uaf"
  "$ROOT/target/demo-sweep"
)
# The variable that moves each directory above somewhere else, in the same
# order. A moved directory is never removed, because its path can be anything,
# / or $HOME included; it is named instead.
target_result_overrides=(
  DEMO07_ARTIFACTS
  DEMO08_ARTIFACTS
  DEMO_SWEEP_LOG_DIR
)
target_provisioned=(
  "$ROOT/target/qemu-busybox"
)

remove_path() {
  local path="$1"
  [ -e "$path" ] || [ -L "$path" ] || return 0
  local rel="${path#"$ROOT"/}"
  if [ "$dry_run" -eq 1 ]; then
    printf '  would remove %s\n' "$rel"
  else
    rm -rf -- "${path:?}"
    printf '  removed %s\n' "$rel"
  fi
}

if [ "$dry_run" -eq 1 ]; then
  printf 'Dry run (no files will be deleted).\n'
fi

if [ -d "$ASSETS" ]; then
  printf 'Cleaning computed demo results under %s\n' "${ASSETS#"$ROOT"/}"
  for name in "${computed[@]}"; do
    remove_path "$ASSETS/$name"
  done

  # $glob is unquoted so that it expands, to nothing when nothing matches; the
  # patterns contain no spaces, so splitting leaves each one a single word.
  shopt -s nullglob
  for glob in "${transient_globs[@]}"; do
    for path in "$ASSETS"/$glob; do
      remove_path "$path"
    done
  done
  shopt -u nullglob
else
  printf 'No QEMU asset directory at %s\n' "${ASSETS#"$ROOT"/}"
fi

for index in "${!target_results[@]}"; do
  path="${target_results[$index]}"
  variable="${target_result_overrides[$index]}"
  moved="${!variable:-}"
  # Drop trailing slashes, so that the default spelled with one is the default.
  while [ "${#moved}" -gt 1 ] && [ "${moved%/}" != "$moved" ]; do
    moved="${moved%/}"
  done
  # An empty value means the default, as it does in the demos. -ef also
  # recognizes the default reached through a symbolic link; it must be tested
  # before the default is removed.
  if [ -n "$moved" ] && [ "$moved" != "$path" ] && ! [ "$moved" -ef "$path" ]; then
    printf '  not removed: %s=%s (clean.sh removes only %s; delete that directory yourself when you no longer need it)\n' \
      "$variable" "$moved" "${path#"$ROOT"/}"
  fi
  remove_path "$path"
done

if [ "$distclean" -eq 1 ]; then
  printf 'Removing the downloaded kernel and built initramfs (--distclean)\n'
  if [ -d "$ASSETS" ]; then
    for name in "${provisioned[@]}"; do
      remove_path "$ASSETS/$name"
    done
  fi
  for path in "${target_provisioned[@]}"; do
    remove_path "$path"
  done
fi

if [ "$dry_run" -eq 1 ]; then
  printf 'Dry run complete.\n'
elif [ "$distclean" -eq 1 ]; then
  printf 'distclean complete: computed results and provisioned assets removed.\n'
else
  printf 'clean complete: computed results removed (kernel and initramfs kept).\n'
fi
