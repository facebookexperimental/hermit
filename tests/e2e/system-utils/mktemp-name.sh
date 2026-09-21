#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# coreutils `mktemp -u` fills the XXXXXX template with a random suffix that
# glibc's __gen_tempname draws from the kernel CSPRNG (getrandom(2), falling
# back to /dev/urandom), so the emitted name varies natively run to run but is
# determinized by Hermit. `-u` (dry-run) means no file is created, so this stays
# a single fast process. getrandom entropy is unique per run, so the native
# control never collides the way a 1-second-granularity clock channel would.
set -euo pipefail

case ${1:-} in
    --prepare) command -v mktemp >/dev/null ;;
    --run) exec mktemp -u "${E2E_TMPDIR:-/tmp}/hermit-mktemp-XXXXXXXXXXXX" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
