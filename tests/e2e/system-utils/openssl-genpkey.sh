#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end OpenSSL asymmetric key-generation determinism fixture.
#
# `openssl genpkey -algorithm ED25519` generates a fresh Ed25519 private key.
# The 32-byte seed is drawn from libcrypto's userspace DRBG, which is itself
# seeded from the kernel CSPRNG (getrandom(2) / /dev/urandom), so a different
# key -- and thus different PEM -- is emitted on every native run. Hermit
# determinizes that kernel entropy at the syscall boundary, so under --strict
# the DRBG output, the generated key, and the emitted PEM all become a pure
# function of the deterministic guest state and repeat bitwise across runs.
#
# This exercises a distinct real-world operation from `openssl rand` (raw RNG
# bytes): a full asymmetric keypair generation as used for TLS/SSH identities.
# It is a single fast process writing PEM to stdout, with no filesystem
# dependency (Hermit isolates the guest /tmp per repeat).
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
        exec openssl genpkey -algorithm ED25519
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
