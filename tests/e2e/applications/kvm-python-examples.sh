#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

readonly ROOT="${BASH_SOURCE[0]%/*}/../../.."

case ${1:-} in
    --prepare) test -x /usr/bin/python3 ;;
    --run)
        "$ROOT/examples/rand.py"
        exec "$ROOT/examples/timed-progress-bar.py"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
