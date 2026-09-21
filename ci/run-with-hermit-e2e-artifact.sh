#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Verify a published artifact, export exact paths, then run one consumer.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
require_install=0
if [[ ${1:-} == --require-install ]]; then
    require_install=1
    shift
fi
[[ $# -gt 0 ]] || { echo "usage: $0 [--require-install] COMMAND..." >&2; exit 2; }
pointer=${HERMIT_E2E_ARTIFACT_POINTER:-$ROOT_DIR/target/ci/hermit-e2e-artifact.path}
bundle=$("$ROOT_DIR/ci/verify-hermit-e2e-artifact.sh" "$pointer")
export HERMIT_BIN="$bundle/hermit"
if [[ -d $bundle/install ]]; then
    export HERMIT_INSTALL_DIR="$bundle/install"
elif ((require_install)); then
    echo "run-with-hermit-e2e-artifact.sh: consumer requires a complete resource bundle: $bundle" >&2
    exit 2
else
    unset HERMIT_INSTALL_DIR
fi
printf 'hermit-e2e-artifact: verified bin=%s install=%s\n' \
    "$HERMIT_BIN" "${HERMIT_INSTALL_DIR:-none}" >&2
exec "$@"
