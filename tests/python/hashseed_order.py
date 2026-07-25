# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# NONDET_SOURCE: getrandom / PYTHONHASHSEED
#
# CPython randomizes string hashing once per process using a seed drawn from the
# operating system (getrandom(2) / urandom), unless PYTHONHASHSEED pins it. That
# seed decides which hash bucket each string lands in, so the iteration order of
# a `set` or `dict` keyed by strings changes from one native run to the next.
#
# Under `hermit run --strict` the seed source (getrandom) is virtualized to a
# deterministic value, so the iteration order becomes a stable, reproducible
# function of the inputs. Run this script twice natively and the two lines below
# differ; run it twice under Hermit and they match.
#
# Intended to be launched with `-S -I` so the result never depends on ambient
# environment variables (e.g. a pinned PYTHONHASHSEED) -- `-I` makes CPython
# ignore the environment, so hash randomization is always active natively.

words = [
    "apple",
    "banana",
    "cherry",
    "date",
    "elderberry",
    "fig",
    "grape",
    "honeydew",
    "kiwi",
    "lemon",
    "mango",
    "nectarine",
]

# A `set` of strings is stored by hash bucket, so its iteration order tracks the
# per-process hash seed and is nondeterministic across native runs.
#
# Note: a `dict` is NOT a useful demonstrator here -- since CPython 3.7 dicts
# preserve insertion order, so their iteration order is deterministic regardless
# of the hash seed. The nondeterminism lives in hash-bucket ordering (sets, and
# set-like views), which is what we print.
print("set:", " ".join(iter(set(words))))
