#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# `openssl passwd -6` computes a crypt(3) SHA-512 password hash. With no -salt
# given, openssl draws a random 16-character salt from libcrypto's userspace
# DRBG, which is seeded from the kernel CSPRNG (getrandom(2), /dev/urandom
# fallback). The salt -- and therefore the whole `$6$salt$hash` string -- varies
# natively run to run but is determinized by Hermit, which virtualizes that
# entropy channel. This is the password-KDF companion to the raw-RNG
# (openssl-rand), asymmetric-keygen (openssl-genpkey), and symmetric-cipher
# (openssl-enc) openssl fixtures. Password and algorithm are inline args, so this
# is a single fast process with no file. DRBG entropy is unique per run, so the
# native control never collides the way a 1-second-granularity clock channel
# would.
set -euo pipefail

case ${1:-} in
    --prepare) command -v openssl >/dev/null ;;
    --run) exec openssl passwd -6 hermit-determinism-corpus ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
