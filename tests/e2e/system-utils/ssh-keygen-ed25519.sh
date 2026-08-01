#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# `ssh-keygen -t ed25519` generates an Ed25519 keypair. OpenSSH draws the 32-byte
# private seed from its own CSPRNG, which is seeded from the kernel entropy pool
# (getrandom(2), /dev/urandom fallback). The public key is a pure function of
# that seed, so the emitted `ssh-ed25519 <base64>` line varies natively run to
# run but is determinized by Hermit, which virtualizes that entropy channel.
# This exercises a distinct real-application path from the openssl fixtures: the
# OpenSSH toolchain's own RNG feeding Ed25519 key derivation, not libcrypto's
# DRBG (openssl-rand / openssl-genpkey) or the raw kernel device
# (random-device). The comment is emptied (-C "") and only the key type and
# material are printed, so no host name or timestamp can leak into the output.
set -euo pipefail

case ${1:-} in
    --prepare) command -v ssh-keygen >/dev/null ;;
    --run)
        work="${E2E_TMPDIR:-/tmp}/hermit-ssh-keygen-ed25519"
        rm -rf "$work"
        mkdir -p "$work"
        ssh-keygen -t ed25519 -N "" -C "" -f "$work/id" >/dev/null 2>&1
        # Print only the key type and base64 material: a deterministic function
        # of the determinized random seed, with no comment/host/time fields.
        awk '{print $1, $2}' "$work/id.pub"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
