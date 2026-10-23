#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        test "$LC_ALL" = C
        test "$TZ" = UTC
        printf 'kvm-shell:%s:%s\n' "$LC_ALL" "$TZ"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
