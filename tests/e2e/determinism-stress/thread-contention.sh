#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare)
        ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/thread_contention.c" \
            -o "$E2E_FIXTURE_DIR/thread-contention"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/thread_stress.c" \
            -o "$E2E_FIXTURE_DIR/thread-stress"
        cc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
            "$ROOT_DIR/tests/e2e/determinism-stress/mmap_fork_shared.c" \
            -o "$E2E_FIXTURE_DIR/mmap-fork-shared"
        ;;
    --run)
        "$E2E_FIXTURE_DIR/thread-contention" contention
        "$E2E_FIXTURE_DIR/thread-contention" epoll
        "$E2E_FIXTURE_DIR/thread-stress"
        exec "$E2E_FIXTURE_DIR/mmap-fork-shared"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
