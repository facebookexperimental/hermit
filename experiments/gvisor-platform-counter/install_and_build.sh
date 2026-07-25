#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 GVISOR_CHECKOUT" >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
gvisor_root=$(realpath -- "$1")
source_dir="$script_dir/gvisor-overlay/counter"
dest_dir="$gvisor_root/pkg/sentry/platform/counter"

if [[ ! -f "$gvisor_root/MODULE.bazel" || ! -f "$gvisor_root/pkg/sentry/platform/platform.go" ]]; then
  echo "not a gVisor checkout: $gvisor_root" >&2
  exit 2
fi

install -D -m 0644 "$source_dir/main.go" "$dest_dir/main.go"
install -D -m 0644 "$source_dir/BUILD" "$dest_dir/BUILD"

bazel=${BAZEL:-bazelisk}
if ! command -v "$bazel" >/dev/null 2>&1 && [[ ! -x "$bazel" ]]; then
  echo "Bazel launcher not found: $bazel" >&2
  exit 2
fi

proxy=()
if command -v with-proxy >/dev/null 2>&1; then
  proxy=(with-proxy)
fi

extra_flags=()
if [[ -n ${GVISOR_BAZEL_FLAGS:-} ]]; then
  read -r -a extra_flags <<<"$GVISOR_BAZEL_FLAGS"
fi

cd -- "$gvisor_root"
exec "${proxy[@]}" "$bazel" build \
  --config=x86_64 \
  -c opt \
  "${extra_flags[@]}" \
  //pkg/sentry/platform/counter:counter
