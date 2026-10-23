#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Perl auto-seeds its `rand` PRNG on first use from a mix that, on Linux, draws
# bytes from the kernel CSPRNG (/dev/urandom, or getrandom(2)). The random
# values therefore vary natively run to run but are determinized by Hermit,
# which virtualizes that entropy channel; the interleaved arithmetic and sort
# are a deterministic control that must stay byte-identical either way. The
# program is passed inline with `-e`, so this is a single fast process with no
# file (avoiding Hermit's per-run isolated /tmp). getrandom/urandom entropy is
# unique per run, so the native control never collides the way a
# 1-second-granularity clock channel would.
set -euo pipefail

prog='my @r = map { int(rand(1000000)) } 1..4;'
prog+=' my $s = 0; $s += $_ * $_ for 1..100;'
prog+=' my $w = join("-", sort qw(hermit determinism perl));'
prog+=' print "rand=" . join(",", @r) . " sumsq=$s sorted=$w\n";'

case ${1:-} in
    --prepare) command -v perl >/dev/null ;;
    --run) exec perl -e "$prog" ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
