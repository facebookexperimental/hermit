#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# util-linux `uuidgen -r` emits a version-4 (random) RFC 4122 UUID whose 122
# random bits libuuid draws from the kernel CSPRNG (getrandom(2), falling back
# to /dev/urandom). The UUID therefore varies natively run to run but is
# determinized by Hermit, which virtualizes that entropy channel. `-r` needs no
# file and forks no child, so this stays a single fast process. getrandom
# entropy is unique per run, so the native control never collides the way a
# 1-second-granularity clock channel would.
set -euo pipefail

case ${1:-} in
    --prepare) command -v uuidgen >/dev/null ;;
    --run) exec uuidgen -r ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
