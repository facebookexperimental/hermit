#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Since Perl 5.18, hash key iteration order is randomized per interpreter run: the
# hash seed is drawn from the kernel CSPRNG (getrandom(2), /dev/urandom fallback)
# at startup and perturbs the bucket layout, so `keys %h` returns a different
# order each native run. This is a canonical real-world reproducibility pitfall --
# it was introduced deliberately as a security hardening measure and routinely
# breaks scripts that assume a stable key order. Hermit determinizes the entropy
# channel that seeds the hash, so the iteration order repeats bitwise under
# `--strict --verify`. This is a DISTINCT nondeterminism source from perl-random
# (the built-in rand() PRNG): here it is the interpreter's hash seed, not a random
# number generator. The interleaved arithmetic control (sumsq) and the sorted-key
# control (which imposes a fixed order) must stay byte-identical either way,
# isolating the unsorted `keys %h` order as the sole field that varies natively.
# The program is passed inline with -e, so this is a single fast process, no file.
set -euo pipefail

prog='my %h = map { $_ => length($_) } qw(alpha bravo charlie delta echo foxtrot golf hotel india);
my @order = keys %h;
my $sum = 0; $sum += $_ * $_ for 1..100;
print "order=" . join(",", @order) . " sumsq=$sum ctl=" . join("", sort keys %h) . "\n";'

case ${1:-} in
    --prepare) command -v perl >/dev/null ;;
    --run) exec perl -e "$prog" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
