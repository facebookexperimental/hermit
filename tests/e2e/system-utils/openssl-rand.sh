#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end OpenSSL entropy determinism fixture.
#
# `openssl rand` draws from libcrypto's userspace DRBG, which is seeded from the
# kernel CSPRNG (getrandom / /dev/urandom) at first use and then produces bytes
# via an AES-CTR construction. Natively the seed differs every run, so the hex
# output varies. Under Hermit --strict the seeding syscall is determinized, so
# the DRBG is seeded identically and its output is bitwise-identical across
# repeat runs. This exercises a distinct, higher-level entropy path than the raw
# /dev/urandom read covered by system-utils/random-device: it proves the guest's
# own PRNG becomes reproducible once its seed source is determinized.
set -euo pipefail

case ${1:-} in
    --prepare)
        command -v openssl >/dev/null 2>&1 || {
            echo "openssl not found" >&2
            exit 1
        }
        exit 0
        ;;
    --run)
        # A single openssl process keeps the workload fast enough for the
        # debug-binary portable CI lane while still crossing the entropy channel.
        exec openssl rand -hex 32
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
