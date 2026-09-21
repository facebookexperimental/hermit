#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare) test -x /usr/bin/python3 ;;
    --run) exec "${BASH_SOURCE[0]%/*}/../../../examples/rand.py" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
