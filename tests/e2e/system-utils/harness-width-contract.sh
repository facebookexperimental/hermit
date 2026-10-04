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
        expected=1
        observed=${HERMIT_E2E_SCHEDULED_JOBS:-missing}
        if [[ $observed != "$expected" ]]; then
            printf 'system-utils-width expected=%s observed=%s\n' "$expected" "$observed"
            exit 1
        fi
        printf 'system-utils-width=%s\n' "$observed"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
