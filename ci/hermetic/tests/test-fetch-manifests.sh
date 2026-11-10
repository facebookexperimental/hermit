#!/usr/bin/env bash
# The fetch phase must fetch every workspace lockfile the offline phase
# builds against. Reverie's own lockfile joined that list after a fresh
# Cargo home failed test.liteinst_allocator: Hermit's root lock does not
# carry Reverie's dev-dependency graph (io-uring, for reverie-kvm).
#
# ⚠️ THIS RUNS THE REAL SCRIPT'S DRY RUN, NOT A COPY of its manifest list.
# A test that re-listed the manifests would only prove the test agrees with
# itself; the dry run prints exactly what the fetch phase would execute.
set -uo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$HERE/../../.." && pwd)

failures=0
check() { # check <description> <condition-result>
    if [[ $2 -eq 0 ]]; then echo "ok   - $1"; else echo "FAIL - $1"; failures=$((failures+1)); fi
}

out=$("$ROOT/ci/hermetic/run-split-validate.sh" --fetch-only --dry-run 2>&1)
rc=$?
check "dry run succeeds" "$rc"

for manifest in Cargo.toml reverie/Cargo.toml liteinst-runtime-build/Cargo.toml agent-utils/rs/Cargo.toml; do
    # The Agent Utils fetch prints its manifest as an absolute path (its
    # fetch runs from a neutral cwd), so match the manifest as a suffix.
    grep -q "cargo fetch --locked --manifest-path .*$manifest" <<<"$out"
    check "fetch phase fetches $manifest" "$?"
done

# Both-ways: the check above must be able to fail. A list printed without
# the Reverie line (the pre-fix shape) is exactly what this guards against,
# so confirm the grep target is the Reverie line and not a substring of
# another manifest's line.
grep -qx "   CARGO_HOME=.* cargo fetch --locked --manifest-path reverie/Cargo.toml" <<<"$out"
check "reverie line is a fetch line of its own" "$?"

if [[ $failures -ne 0 ]]; then
    echo "test-fetch-manifests: $failures failure(s)" >&2
    exit 1
fi
echo "test-fetch-manifests: all checks passed"
