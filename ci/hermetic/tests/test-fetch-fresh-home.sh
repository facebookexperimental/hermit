#!/usr/bin/env bash
# Fresh-Cargo-home proof for the pinned-root fetch phase: with an EMPTY
# Cargo home, fetching Reverie's own lockfile must bring down io-uring, the
# Reverie dev-dependency whose absence failed test.liteinst_allocator when
# the fetch phase only fetched Hermit's root lock.
#
# This is the slow, networked companion to test-fetch-manifests.sh (which
# only checks the dry-run list): it performs the one fetch that regressed,
# into a throwaway Cargo home, and asserts the crate file is in the cache.
# Run it where the fetch phase itself may run (host, with network); it is
# deliberately not part of the offline phase.
set -uo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)

failures=0
check() { # check <description> <condition-result>
    if [[ $2 -eq 0 ]]; then echo "ok   - $1"; else echo "FAIL - $1"; failures=$((failures+1)); fi
}

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
export CARGO_HOME="$work/cargo"
mkdir -p "$CARGO_HOME"

# Keep this host's proxy route, as the fetch phase does.
if [[ -z ${CARGO_HTTP_PROXY+x} && -n ${https_proxy:-${HTTPS_PROXY:-}} ]]; then
    export CARGO_HTTP_PROXY="${https_proxy:-${HTTPS_PROXY:-}}"
fi

(cd "$ROOT" && cargo fetch --locked --manifest-path reverie/Cargo.toml) >"$work/fetch.log" 2>&1
check "fresh-home fetch of reverie/Cargo.toml succeeds" "$?"

compgen -G "$CARGO_HOME/registry/cache/*/io-uring-*.crate" >/dev/null
check "io-uring crate is in the fresh Cargo home cache" "$?"

if [[ $failures -ne 0 ]]; then
    echo "test-fetch-fresh-home: $failures failure(s); fetch log:" >&2
    tail -5 "$work/fetch.log" >&2
    exit 1
fi
echo "test-fetch-fresh-home: all checks passed"
