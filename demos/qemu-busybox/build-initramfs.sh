#!/usr/bin/env bash

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)
busybox=${BUSYBOX:-}
output=${1:-$repo_root/target/qemu-busybox/initramfs-busybox.cpio.gz}

if [[ -z $busybox ]]; then
  busybox=$(command -v busybox || true)
fi
[[ -n $busybox && -x $busybox ]] || fail \
  "statically linked BusyBox not found; install it or set BUSYBOX"
file "$busybox" | grep -q 'statically linked' || fail \
  "BusyBox must be statically linked: $busybox"

for command in cpio file find gzip install sha256sum sort stat touch wc; do
  command -v "$command" >/dev/null || fail "$command is required"
done
for applet in bc ls mknod mount poweroff sha256sum sh uname; do
  "$busybox" --list | grep -Fxq "$applet" || fail \
    "BusyBox does not provide required applet: $applet"
done

mkdir -p "$repo_root/target/qemu-busybox" "$(dirname -- "$output")"
root=$(mktemp -d "$repo_root/target/qemu-busybox/root.XXXXXX")
cleanup() {
  rm -rf -- "$root"
}
trap cleanup EXIT

mkdir -p "$root"/{bin,dev,etc,home,proc,root,sbin,sys,tmp,usr}
install -m 0755 "$busybox" "$root/bin/busybox"
install -m 0755 "$script_dir/init" "$root/init"
printf 'root:x:0:0:root:/:/bin/sh\n' >"$root/etc/passwd"
printf 'root:x:0:\n' >"$root/etc/group"

while IFS= read -r applet; do
  [[ $applet == bin/busybox ]] && continue
  mkdir -p "$root/$(dirname -- "$applet")"
  ln -s /bin/busybox "$root/$applet"
done < <("$busybox" --list-full)

# Fix every archive input that otherwise varies across hosts or invocations.
find "$root" -exec touch -h -d @0 {} +
(
  cd "$root"
  find . -print0 | sort -z | \
    cpio --quiet --null --create --format=newc --owner=0:0 --reproducible
) | gzip -n -9 >"$output"

printf 'busybox=%s\nbusybox_sha256=%s\ninitramfs=%s\ninitramfs_sha256=%s\nentries=%s\nbytes=%s\n' \
  "$busybox" "$(sha256sum "$busybox" | cut -d' ' -f1)" \
  "$output" "$(sha256sum "$output" | cut -d' ' -f1)" \
  "$(gzip -dc "$output" | cpio -it 2>/dev/null | wc -l)" \
  "$(stat -c %s "$output")"
