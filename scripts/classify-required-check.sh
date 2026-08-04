#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Compatibility entry point for the canonical Python status classifier.
set -euo pipefail

if (($# != 2)); then
    echo "usage: $0 STATUS CONCLUSION" >&2
    exit 2
fi

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
exec python3 "$root_dir/agent-utils/py/ci_hub_check_outcome.py" \
    --status "$1" --conclusion "$2"
