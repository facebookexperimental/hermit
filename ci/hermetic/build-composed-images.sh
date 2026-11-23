#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Build and load every composed mini-family image from the committed
# lock, then verify each loaded manifest digest against its receipt.
# This is the provisioning path for hosts that run switched buckets:
# a host without a mapped image must fail here (or at validate
# admission when run), never silently use another image.
#
# Usage: build-composed-images.sh [--check]
#   --check  verify every receipted image is already present by exact
#            digest; build and load nothing.
# Requires: nix (flakes), podman. Network via with-proxy when present.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)

if [[ -v CONTAINERS_STORAGE_CONF || -v CONTAINERS_CONF_OVERRIDE ]]; then
    echo "build-composed-images: unset CONTAINERS_STORAGE_CONF/CONTAINERS_CONF_OVERRIDE (would load into another store than the pinned root uses)" >&2
    exit 2
fi
command -v podman >/dev/null || {
    echo "build-composed-images: podman is required" >&2
    exit 2
}
read -r graph_root < <(podman info --format '{{.Store.GraphRoot}}') || {
    echo "build-composed-images: podman is unavailable" >&2
    exit 2
}
if [[ "$graph_root" != "$HOME/.local/share/containers/storage" ]]; then
    echo "build-composed-images: unexpected podman graph root $graph_root" >&2
    exit 2
fi

runner=()
if command -v with-proxy >/dev/null 2>&1; then
    runner=(with-proxy)
fi
NIX=(nix --extra-experimental-features "nix-command flakes")

# image short name -> receipt file. The loaded digest must equal the
# receipt or the host is not provisioned for that family.
names=(mini mini-python mini-runner mini-crypto mini-suite)

receipt_field() {
    # $1 receipt file, $2 dotted field without leading dot (jq-style,
    # only image.manifest_digest / image.reference are used).
    python3 - "$1" "$2" <<'PY'
import json, sys
value = json.load(open(sys.argv[1]))
for part in sys.argv[2].split("."):
    value = value[part]
print(value)
PY
}

check_only=0
if [[ ${1:-} == --check ]]; then
    check_only=1
elif [[ $# -ne 0 ]]; then
    echo "usage: build-composed-images.sh [--check]" >&2
    exit 2
fi

for short in "${names[@]}"; do
    receipt="$HERE/images/$short.json"
    if [[ ! -f "$receipt" ]]; then
        echo "build-composed-images: missing receipt $receipt" >&2
        exit 1
    fi
    reference=$(receipt_field "$receipt" image.reference)
    if ((check_only == 0)); then
        out=(
            "${runner[@]}"
            "${NIX[@]}" build --no-link --print-out-paths
            --option sandbox true
            ".#hermeticImages.x86_64-linux.$short"
        )
        echo "build-composed-images: building $short"
        out_path=$(cd "$HERE" && "${out[@]}")
        echo "build-composed-images: loading $reference from $out_path (RepoTags verified by digest check below; never over hermit-hermetic-validate)"
        podman load -i "$out_path" >/dev/null
    fi
    if ! podman image exists "$reference"; then
        echo "build-composed-images: image $reference is not present locally" >&2
        exit 1
    fi
    loaded=$(podman image inspect "$reference" --format '{{.Digest}}')
    recorded=$(receipt_field "$receipt" image.manifest_digest)
    if [[ "$loaded" != "$recorded" ]]; then
        echo "build-composed-images: loaded digest $loaded != receipt $recorded for $short" >&2
        exit 1
    fi
    echo "build-composed-images: OK $short $loaded"
done
echo "build-composed-images: all ${#names[@]} composed images present by exact digest"
