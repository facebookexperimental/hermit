#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# `openssl req -x509 -newkey ec` issues a self-signed X.509 certificate in one
# shot. That single artifact folds together several native entropy/time channels:
# the fresh EC private key and the ECDSA signature nonce both come from
# libcrypto's userspace DRBG (seeded from the kernel CSPRNG via getrandom(2)),
# the certificate serial number is a random 64-bit integer from the same DRBG,
# and the notBefore/notAfter validity dates come from the wall clock. So the whole
# PEM certificate varies natively run to run but is determinized by Hermit, which
# virtualizes both the entropy channel and the clock. This is the
# certificate-issuance companion to the raw-RNG (openssl-rand), password-KDF
# (openssl-passwd), asymmetric-keygen (openssl-genpkey), and symmetric-cipher
# (openssl-enc) openssl fixtures -- a realistic reproducible-PKI workload rather
# than a single primitive. The key is discarded to /dev/null and the certificate
# is written to stdout, so this is a single fast process with no roundtrip file.
# The random serial and key make the native control unique per run, so it never
# collides the way a 1-second-granularity clock channel would.
set -euo pipefail

case ${1:-} in
    --prepare) command -v openssl >/dev/null ;;
    --run)
        exec openssl req -x509 -newkey ec \
            -pkeyopt ec_paramgen_curve:prime256v1 \
            -nodes -keyout /dev/null \
            -subj /CN=hermit-determinism-corpus -days 365 -out -
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
