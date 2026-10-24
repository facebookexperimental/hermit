#!/usr/bin/env bash
# Provision the pinned Linux kernel and the BusyBox initramfs that demos 5, 6,
# and 7 boot. Run with --check to report missing prerequisites only.

set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$LIB_DIR/../.." && pwd)"
HERMIT_REPO="${HERMIT_REPO:-$ROOT}"
# shellcheck source=qemu-paths.sh
source "$LIB_DIR/qemu-paths.sh"
ARTIFACT_DIR="${QEMU_ASSETS:-$(qemu_default_assets "$ROOT")}"
BUSYBOX="${BUSYBOX:-$(command -v busybox || true)}"
KERNEL_SHA256="${QEMU_KERNEL_SHA256:-e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2}"
DEFAULT_KERNEL_URL="https://github.com/rrnewton/hermit/releases/download/qemu-kernel-$KERNEL_SHA256/bzImage"
KERNEL_URL="${QEMU_KERNEL_URL:-$DEFAULT_KERNEL_URL}"
QEMU="${QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
PYTHON="${QEMU_DEMO_PYTHON:-$(command -v python3 || true)}"
# Bump when the initramfs contents change, so cached copies are rebuilt.
INITRAMFS_VERSION=9
INITRAMFS_VERSION_FILE="$ARTIFACT_DIR/.initramfs-version"
# The build record: one line, "<INITRAMFS_VERSION> <SHA-256>", naming the
# initramfs this script built by its SHA-256 and the version it built it at.
# Checkouts of different versions may share ARTIFACT_DIR, so the version file
# and the archive can each be replaced at any time; the record names the bytes
# it is about, so it stays true whatever replaces the archive later. Demo 5
# boots a private copy of the archive and records the version only when this
# record names that copy's SHA-256 (demo_common.booted_initramfs_producer).
INITRAMFS_BUILD_FILE="$ARTIFACT_DIR/.initramfs-build"
CHECK_ONLY=0

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

case "${1:-}" in
  "") ;;
  --check) CHECK_ONLY=1 ;;
  *) fail "usage: $0 [--check]" ;;
esac

size_mb() {
  awk -v bytes="$1" 'BEGIN { printf "%.1f", bytes / 1000000 }'
}

available_executable() {
  [ -n "$1" ] || return 1
  case "$1" in
    */*) [ -x "$1" ] ;;
    *) command -v "$1" >/dev/null 2>&1 ;;
  esac
}

# shellcheck source=fetch-url.sh
source "$LIB_DIR/fetch-url.sh"

preflight() {
  local issue
  local -a issues=()

  available_executable "$QEMU" || \
    issues+=("missing qemu-system-x86_64 (or set QEMU_BIN=/path/to/qemu)")
  command -v qemu-img >/dev/null 2>&1 || \
    issues+=("missing qemu-img")
  available_executable "$PYTHON" || \
    issues+=("missing Python 3 (or set QEMU_DEMO_PYTHON=/path/to/python3)")

  for tool in file cpio gzip sed sha256sum touch; do
    command -v "$tool" >/dev/null 2>&1 || \
      issues+=("missing required tool: $tool")
  done

  if [ -z "$BUSYBOX" ] || [ ! -x "$BUSYBOX" ]; then
    issues+=("missing statically linked BusyBox (or set BUSYBOX=/path/to/busybox)")
  elif command -v file >/dev/null 2>&1 \
       && ! file "$BUSYBOX" | grep -q 'statically linked'; then
    issues+=("BUSYBOX is not statically linked: $BUSYBOX")
  fi

  if [ -n "${KERNEL_IMAGE:-}" ]; then
    [ -r "$KERNEL_IMAGE" ] || issues+=("unreadable KERNEL_IMAGE: $KERNEL_IMAGE")
  elif [ -n "$KERNEL_URL" ]; then
    command -v curl >/dev/null 2>&1 || \
      issues+=("missing curl for QEMU_KERNEL_URL")
  else
    issues+=("no kernel source; set KERNEL_IMAGE or QEMU_KERNEL_URL")
  fi

  [[ $KERNEL_SHA256 =~ ^[0-9a-f]{64}$ ]] || \
    issues+=("QEMU_KERNEL_SHA256 must be a lowercase 64-character SHA-256")

  if [ "${#issues[@]}" -ne 0 ]; then
    printf 'QEMU demo dependency check failed (%d issues):\n' \
      "${#issues[@]}" >&2
    for issue in "${issues[@]}"; do
      printf '  - %s\n' "$issue" >&2
    done
    printf '\nDebian/Ubuntu: sudo apt install python3 qemu-system-x86 qemu-utils busybox-static cpio gzip curl file\n' >&2
    printf 'Fedora: sudo dnf install python3 qemu-system-x86-core qemu-img busybox cpio gzip curl file\n' >&2
    printf 'CentOS/RHEL: install qemu-kvm-core, qemu-img, and EPEL busybox; set QEMU_BIN and BUSYBOX if their paths differ.\n' >&2
    return 1
  fi

  if [ "$CHECK_ONLY" -eq 1 ]; then
    echo 'QEMU dependency check passed: qemu-system-x86_64 qemu-img python3 static-busybox file cpio gzip sed sha256sum touch kernel-source'
  fi
}

preflight || exit 1
[ "$CHECK_ONLY" -eq 0 ] || exit 0

mkdir -p "$ARTIFACT_DIR" "$HERMIT_REPO/target"

kernel_tmp=""
initrd_tmp=""
version_tmp=""
build_tmp=""
workdir=""
cleanup() {
  [ -z "$kernel_tmp" ] || rm -f "$kernel_tmp"
  [ -z "$initrd_tmp" ] || rm -f "$initrd_tmp"
  [ -z "$version_tmp" ] || rm -f "$version_tmp"
  [ -z "$build_tmp" ] || rm -f "$build_tmp"
  [ -z "$workdir" ] || rm -rf "$workdir"
}
trap cleanup EXIT

cached_kernel_sha=""
if [ -r "$ARTIFACT_DIR/bzImage" ]; then
  cached_kernel_sha="$(sha256sum "$ARTIFACT_DIR/bzImage" | cut -d' ' -f1)"
fi

if [ "$cached_kernel_sha" != "$KERNEL_SHA256" ]; then
  if [ -n "$cached_kernel_sha" ]; then
    printf 'kernel: replacing cache with unexpected sha256 %s\n' \
      "$cached_kernel_sha"
  fi
  kernel_tmp="$ARTIFACT_DIR/.bzImage.$$"
  if [ -n "${KERNEL_IMAGE:-}" ]; then
    [ -r "$KERNEL_IMAGE" ] || fail "unreadable KERNEL_IMAGE: $KERNEL_IMAGE"
    cp "$KERNEL_IMAGE" "$kernel_tmp"
    kernel_source="$KERNEL_IMAGE"
  elif [ -n "$KERNEL_URL" ]; then
    echo 'Downloading kernel...'
    fetch_url "$KERNEL_URL" "$kernel_tmp" || \
      fail "kernel download failed: $KERNEL_URL"
    kernel_source="$KERNEL_URL"
  else
    fail "set KERNEL_IMAGE or QEMU_KERNEL_URL"
  fi

  downloaded_kernel_sha="$(sha256sum "$kernel_tmp" | cut -d' ' -f1)"
  if [ "$downloaded_kernel_sha" != "$KERNEL_SHA256" ]; then
    fail "kernel sha256 mismatch from $kernel_source: expected $KERNEL_SHA256, got $downloaded_kernel_sha"
  fi
  mv "$kernel_tmp" "$ARTIFACT_DIR/bzImage"
  kernel_tmp=""
  kernel_bytes="$(stat -c%s "$ARTIFACT_DIR/bzImage")"
  printf 'Kernel ready (%sMB)\n' "$(size_mb "$kernel_bytes")"
else
  kernel_bytes="$(stat -c%s "$ARTIFACT_DIR/bzImage")"
  printf 'Kernel ready (%sMB, cached)\n' "$(size_mb "$kernel_bytes")"
fi

cached_initramfs_version="$(cat "$INITRAMFS_VERSION_FILE" 2>/dev/null || true)"
cached_initramfs_build="$(cat "$INITRAMFS_BUILD_FILE" 2>/dev/null || true)"
cached_initramfs_sha=""
if [ -r "$ARTIFACT_DIR/initramfs.cpio.gz" ]; then
  cached_initramfs_sha="$(sha256sum "$ARTIFACT_DIR/initramfs.cpio.gz" | cut -d' ' -f1)"
fi
# A cached archive is reused only when the build record names its SHA-256 at
# this version. An archive that a checkout without build records built, or that
# was replaced after its record was written, is rebuilt, and gets a record.
if [ ! -r "$ARTIFACT_DIR/initramfs.cpio.gz" ] || \
   [ "$cached_initramfs_version" != "$INITRAMFS_VERSION" ] || \
   [ "$cached_initramfs_build" != "$INITRAMFS_VERSION $cached_initramfs_sha" ]; then
  workdir="$(mktemp -d "$HERMIT_REPO/target/qemu-demo-assets.XXXXXX")"
  root="$workdir/initramfs"
  mkdir -p "$root"/{bin,sbin,etc,proc,sys,dev,tmp,usr/bin,usr/sbin}
  cp "$BUSYBOX" "$root/bin/busybox"
  chmod +x "$root/bin/busybox"

  # Every command /init runs, other than the shell's own builtins, must be a
  # BusyBox applet: a missing one would fail only inside the guest, after the
  # boot snapshot is taken. chpst runs the guest's command without root.
  applet_list="$("$root/bin/busybox" --list-full)" || \
    fail "BusyBox could not list its applets: $BUSYBOX"
  missing_applets=()
  for applet in chpst cttyhack date dd head mount setsid sh sleep tr uname; do
    case $'\n'"$applet_list"$'\n' in
      *$'\n'"$applet"$'\n'* | */"$applet"$'\n'*) ;;
      *) missing_applets+=("$applet") ;;
    esac
  done
  [ "${#missing_applets[@]}" -eq 0 ] || \
    fail "BusyBox lacks applets the guest's /init runs (${missing_applets[*]}): $BUSYBOX"

  (
    cd "$root"
    while IFS= read -r applet; do
      mkdir -p "$(dirname "$applet")"
      [ "$applet" = bin/busybox ] || ln -sf /bin/busybox "$applet"
    done <<<"$applet_list"
  )

  # The guest's workload arrives on a small disk (/dev/vda) that is attached
  # before the boot snapshot is taken, so what the guest runs never depends on
  # host timing. Each poll opens and closes the device; the last close drops
  # its cached blocks, so a read after a snapshot restore sees the command the
  # host wrote in the meantime. The loop sleeps rather than spins because
  # demos 5 and 6 run the guest without timer preemption.
  #
  # The command's output is framed so that nothing the command does can be
  # mistaken for the frame. /init prints a BEGIN line naming the frame format,
  # runs the command with its stdout and stderr going to a file, then prints
  # that file with "| " in front of every line, and last an END line carrying
  # the command's exit status. A file rather than a pipe keeps $? the
  # command's own status, starts no extra process before the command runs, so
  # guest pids are unchanged, and lets a background job the command started
  # keep its output open without delaying END; the output therefore appears
  # only after the command has exited.
  #
  # The command cannot write to the console itself. chpst runs it as user and
  # group 1000 with no supplementary groups, and its standard input is
  # /dev/null, so it holds no descriptor for the console. The kernel creates
  # /dev/console and /dev/ttyS* readable and writable by root only and
  # /dev/kmsg writable by root only, and the user cannot signal, trace, or
  # replace /init or the output file (/tmp is sticky). Everything the command
  # prints therefore reaches the console through the loop below, as "| "
  # lines. The END line has byte 0x01 between the marker and "status=", and
  # the loop removes every 0x01 byte from the command's output, so even the
  # tail of a "| " line that a kernel message splits off cannot be an END
  # line. Kernel messages cannot be one either: the kernel command line sets
  # printk.time=1, so every line the kernel prints starts with "[". The
  # placeholder in SEP= is replaced with the 0x01 byte below. The host side is
  # CommandTranscriptParser in demos/lib/qemu_controller.py: change the two
  # together and bump INITRAMFS_VERSION.
  cat >"$root/init" <<'INIT'
#!/bin/sh
mount -t proc     none /proc 2>/dev/null
mount -t sysfs    none /sys  2>/dev/null
mount -t devtmpfs none /dev  2>/dev/null || mount -t tmpfs none /dev 2>/dev/null
echo "=========================================="
echo "HERMIT-QEMU-BASELINE-BOOT-OK"
echo "kernel: $(uname -r)"
echo "rtc: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "=========================================="
echo "HERMIT-QEMU-COMMAND-DISK-READY"
CMDDEV=/dev/vda
while :; do
  CMD=$(dd if="$CMDDEV" bs=512 count=1 2>/dev/null | tr -d '\000' | head -n 1)
  case "$CMD" in
    ""|WAIT) ;;
    *) break ;;
  esac
  sleep 1
done
echo "__HERMIT_COMMAND_BEGIN__ format=3"
chpst -u 1000:1000 sh -c "$CMD" </dev/null >/tmp/.hermit-command-output 2>&1
STATUS=$?
SEP='@HERMIT_FRAME_SEP@'
while IFS= read -r LINE || [ -n "$LINE" ]; do
  OUT=
  while :; do
    case "$LINE" in
      *"$SEP"*) OUT="$OUT${LINE%%"$SEP"*}"; LINE="${LINE#*"$SEP"}" ;;
      *) break ;;
    esac
  done
  printf '| %s\n' "$OUT$LINE"
done </tmp/.hermit-command-output
printf '__HERMIT_COMMAND_END__%sstatus=%s\n' "$SEP" "$STATUS"
echo "Interactive busybox shell. Type 'poweroff -f' to exit."
exec setsid cttyhack sh
INIT
  frame_sep="$(printf '\001')"
  sed -i "s/@HERMIT_FRAME_SEP@/$frame_sep/" "$root/init"
  init_text="$(cat "$root/init")"
  case "$init_text" in
    *@HERMIT_FRAME_SEP@*) fail "the frame separator placeholder is still in /init" ;;
    *"SEP='$frame_sep'"*) ;;
    *) fail "the frame separator byte is missing from /init" ;;
  esac
  chmod +x "$root/init"
  printf 'root:x:0:0:root:/:/bin/sh\n' >"$root/etc/passwd"
  printf 'root:x:0:\n' >"$root/etc/group"

  # Set every mode explicitly, because cpio stores modes as it finds them: the
  # directories, /init, /etc/passwd, and /etc/group were created under the
  # caller's umask, and /bin/busybox was copied with the installed BusyBox's
  # mode. Symbolic links are left alone: their mode is always 0777, and chmod
  # would follow them to /bin/busybox. /tmp is writable by everyone and sticky,
  # so the unprivileged command can create files there but cannot remove or
  # rename /init's output file.
  find "$root" -type d -exec chmod 0755 {} +
  find "$root" -type f -exec chmod 0644 {} +
  chmod 0755 "$root/bin/busybox" "$root/init"
  chmod 1777 "$root/tmp"

  # Build the archive reproducibly: fixed modes, timestamps, and ownership,
  # sorted entries, no inode numbers, and no gzip header timestamp. The same
  # BusyBox binary, cpio, and gzip then yield a byte-identical initramfs
  # whatever the caller's umask, when the build directory is on the same kind
  # of filesystem: cpio also stores each directory's link count as the
  # filesystem reports it (btrfs reports 1, ext4 2 plus the number of
  # subdirectories), and GNU cpio 2.13 has no option to omit it.
  find "$root" -exec touch -h -d @0 {} +
  initrd_tmp="$ARTIFACT_DIR/.initramfs.cpio.gz.$$"
  (
    cd "$root"
    find . -print0 | LC_ALL=C sort -z |
      cpio --quiet --null --create --format=newc --owner=0:0 --reproducible
  ) | gzip -n -9 >"$initrd_tmp"
  # Hashed before the rename, while only this script can have written it.
  initrd_sha="$(sha256sum "$initrd_tmp" | cut -d' ' -f1)"
  mv "$initrd_tmp" "$ARTIFACT_DIR/initramfs.cpio.gz"
  initrd_tmp=""
  version_tmp="$ARTIFACT_DIR/.initramfs-version.$$"
  printf '%s\n' "$INITRAMFS_VERSION" >"$version_tmp"
  mv "$version_tmp" "$INITRAMFS_VERSION_FILE"
  version_tmp=""
  build_tmp="$ARTIFACT_DIR/.initramfs-build.$$"
  printf '%s %s\n' "$INITRAMFS_VERSION" "$initrd_sha" >"$build_tmp"
  mv "$build_tmp" "$INITRAMFS_BUILD_FILE"
  build_tmp=""
  printf 'Initramfs ready (%sMB)\n' \
    "$(size_mb "$(stat -c%s "$ARTIFACT_DIR/initramfs.cpio.gz")")"
else
  printf 'Initramfs ready (%sMB, cached)\n' \
    "$(size_mb "$(stat -c%s "$ARTIFACT_DIR/initramfs.cpio.gz")")"
fi

echo 'QEMU assets ready.'
