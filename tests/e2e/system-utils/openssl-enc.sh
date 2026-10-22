#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# `openssl enc -salt` prepends a random 8-byte salt (drawn from libcrypto's
# DRBG, seeded by the kernel CSPRNG) before deriving the PBKDF2 key, so the
# base64 ciphertext varies natively run to run but is determinized by Hermit.
# This exercises the symmetric-cipher + KDF path, distinct from the raw-bytes
# (openssl-rand) and asymmetric-keygen (openssl-genpkey) coverage. The plaintext
# is written under $E2E_TMPDIR (Hermit isolates /tmp per repeat) and openssl
# runs as a single process reading that file.
set -euo pipefail

case ${1:-} in
    --prepare) command -v openssl >/dev/null ;;
    --run)
        work="${E2E_TMPDIR:-/tmp}/hermit-openssl-enc"
        rm -rf "$work"
        mkdir -p "$work"
        printf 'hermit determinism corpus payload\n' >"$work/pt.txt"
        exec openssl enc -aes-256-cbc -pbkdf2 -salt -pass pass:hermit \
            -base64 -in "$work/pt.txt"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
