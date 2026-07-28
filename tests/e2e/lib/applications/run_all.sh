#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
readonly SCRIPT_DIR

for test_script in sqlite_on_disk.sh sqlite_deep.sh redis_deep.sh http_server.sh build_tools.sh; do
    printf '==> %s\n' "$test_script"
    "$SCRIPT_DIR/$test_script"
done
