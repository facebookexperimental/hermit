#!/usr/bin/env bash
# Guard: hermit-detcore must compile for x86_64-unknown-linux-musl.
#
# The liteinst runtime staticlib is built for musl by the in-guest producer
# route, but no validate node builds that target, so a musl-only type error
# in Detcore (statfs f_type is u64 on musl, i64-typed magics in libc) landed
# invisibly and blocked the producer's consumers. This checker runs the same cargo check
# the producer depends on. It only checks; it never builds the staticlib.
set -euo pipefail

cd "$(dirname "$0")/.."

target="x86_64-unknown-linux-musl"
if ! rustup target list --installed | grep -qx "$target"; then
    # The pinned toolchain may not carry the musl std component. Fetch it
    # through the proxy when available; without it this checker cannot
    # evaluate and reports no-result (75), not a product failure.
    if command -v with-proxy >/dev/null 2>&1; then
        with-proxy rustup target add "$target" || true
    else
        rustup target add "$target" || true
    fi
fi
if ! rustup target list --installed | grep -qx "$target"; then
    echo "check-musl-detcore: NO RESULT -- $target std component unavailable" >&2
    exit 75
fi

cargo check -p hermit-detcore --target "$target"
echo "check-musl-detcore: OK -- hermit-detcore compiles for $target"
