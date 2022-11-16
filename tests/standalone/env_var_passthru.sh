#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

if [ "$*" == "" ]; then
    hermit="hermit"
else
    hermit="$1"
fi

tmpdir=$(mktemp -d env_var_passthru_test_XXXXXXX)
function on_exit {
    rm -rf -- "$tmpdir"
}
trap on_exit EXIT

"$hermit" run --base-env=host /bin/env > "$tmpdir/full_env"
full_size=$(wc -l < "$tmpdir/full_env")

# Our testing environment must be messed up if it has less than 5 entries.
if [ "$full_size" -lt 5 ]; then
    echo "Expected full environment to contain 5 or more variables, got $full_size"
    echo "Environment was: "
    cat "$tmpdir/full_env"
    exit 1
else
    echo "Full environment looks good."
fi

"$hermit" run --base-env=empty /bin/env > "$tmpdir/empty_env"
empty_size=$(wc -l < "$tmpdir/empty_env")
if [ "$empty_size" -gt 2 ]; then
    echo "Expected --base-env=empty environment to contain 2 or fewer variables, got $empty_size"
    echo "Environment was: "
    cat "$tmpdir/empty_env"
    exit 1
else
    echo "Environment under --base=env=empty looks ok."
fi

# Minimal env contains at least HOME, PATH, HOSTNAME currently:
"$hermit" run --base-env=minimal /bin/env > "$tmpdir/minimal_env"
minimal_size=$(wc -l < "$tmpdir/minimal_env")
if [ "$minimal_size" -gt 5 ] || [ "$minimal_size" -lt 3 ]; then
    echo "Expected --base-env=minimal environment to contain 2 or fewer variables, got $minimal_size"
    echo "Environment was: "
    cat "$tmpdir/minimal_env"
    exit 1
else
    echo "Environment under --base=env=minimal looks ok."
fi

export FOO="BAR"
export BAZ="QUUX"
"$hermit" run --base-env=empty --env FOO --env BAZ=33 /bin/env > "$tmpdir/custom_env"
if ! grep -q "FOO=BAR" "$tmpdir/custom_env"; then
    echo "Missing expected FOO=BAR binding in environment:"
    cat "$tmpdir/custom_env"
    exit 1
elif ! grep -q "BAZ=33" "$tmpdir/custom_env"; then
    echo "Missing expected BAZ=33 binding in environment:"
    cat "$tmpdir/custom_env"
    exit 1
else
    echo "Pass through via --env looks ok."
fi

echo "Test succeeded."
