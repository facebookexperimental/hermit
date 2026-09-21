#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end multithreaded-compression determinism fixture.
#
# WHY THIS IS A NEW SURFACE. The corpus already has multithreaded workloads,
# but they are all purpose-built C sentinels under determinism-stress. This is a
# real-world multithreaded APPLICATION: `zstd -T4` spins a worker pool, splits
# the input into jobs, compresses them concurrently, and reassembles the frame
# in order. That exercises pthread creation, a futex-coordinated job queue, and
# per-worker buffers under Hermit's sequentialized scheduler -- while asserting
# something a synthetic thread test cannot: that the ASSEMBLED ARTIFACT is
# byte-identical, not merely that the threads finished.
#
# WHAT IS AND IS NOT BEING CLAIMED. zstd's multithreaded output is deterministic
# by construction natively -- job boundaries are fixed by input offset, not by
# which worker happens to pick a job up -- so this fixture does NOT assert
# native variance, and its `naked` mode is disabled with that reason recorded.
# It is a MECHANISM test: the value is that a thread-pool application driven
# through Hermit's scheduler still produces the identical frame, and that the
# round trip verifies. A regression in thread scheduling, futex handling, or
# buffer reuse would surface as a changed digest or a failed decompression.
#
# The digest is taken over the compressed frame (not just the round trip)
# because a round-trip-only check would pass even if the compressor emitted a
# different-but-valid frame each time, which is exactly the nondeterminism this
# is meant to catch.
#
# Input is generated deterministically from a fixed seed rather than read from
# the filesystem, so the fixture depends on nothing outside the guest and stays
# comfortably inside the portable debug-binary time budget.
set -euo pipefail

# A fixed number of workers, not -T0: `-T0` binds the worker count to the host's
# core count, which differs between the dev box and a hosted runner, and the
# job split -- and therefore the frame -- depends on it. Pinning the thread
# count keeps the expected digest a property of the code under test rather than
# of the machine it ran on.
THREADS=4

case ${1:-} in
    --prepare)
        command -v zstd >/dev/null 2>&1 || {
            echo "zstd not found" >&2
            exit 1
        }
        # Multithreading must actually be compiled in; a single-threaded zstd
        # would silently reduce this to a plain compression test.
        zstd -T2 --version >/dev/null 2>&1 || {
            echo "zstd lacks multithread support" >&2
            exit 1
        }
        exit 0
        ;;
    --run)
        # Deterministic, compressible-but-not-trivial input, generated inline so
        # the fixture reads no host file. Large enough that zstd splits it into
        # several jobs and the worker pool is genuinely used.
        payload=$(awk 'BEGIN{
            s=12345;
            for (i = 0; i < 60000; i++) {
                s = (s * 1103515245 + 12345) % 2147483648;
                printf "%d-%s\n", s % 997, substr("hermitdeterminism", 1 + (s % 8), 9);
            }
        }')

        compressed_digest=$(printf '%s\n' "$payload" \
            | zstd -T"$THREADS" -3 -c 2>/dev/null \
            | sha256sum | cut -d' ' -f1)

        # Round trip must also reproduce the input exactly: a stable digest over
        # a corrupt frame would be a false pass.
        roundtrip_digest=$(printf '%s\n' "$payload" \
            | zstd -T"$THREADS" -3 -c 2>/dev/null \
            | zstd -d -c 2>/dev/null \
            | sha256sum | cut -d' ' -f1)
        input_digest=$(printf '%s\n' "$payload" | sha256sum | cut -d' ' -f1)

        if [[ $roundtrip_digest != "$input_digest" ]]; then
            echo "zstd round trip did not reproduce the input" >&2
            exit 1
        fi

        printf 'threads=%s frame=%s roundtrip=ok input=%s\n' \
            "$THREADS" "${compressed_digest:0:32}" "${input_digest:0:32}"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
