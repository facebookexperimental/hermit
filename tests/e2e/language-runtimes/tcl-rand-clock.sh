#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end Tcl determinism fixture.
#
# Tcl's `expr rand()` seeds its PRNG from the interpreter's pid and the wall
# clock on first use, and `clock seconds`/`clock milliseconds` read the clock.
# Natively both vary every run. Under Hermit --strict the pid is virtualized and
# the clock is pinned to the virtual epoch, so the seed and clock are identical
# across runs and an otherwise deterministic Tcl workload -- list sorting, a
# dict fold, and a file-I/O roundtrip through E2E_TMPDIR -- produces
# bitwise-identical output. The sort/dict results are deterministic by
# construction and cross-check that container semantics are preserved.
set -euo pipefail

case ${1:-} in
    --prepare)
        command -v tclsh >/dev/null 2>&1 || {
            echo "tclsh not found" >&2
            exit 1
        }
        exit 0
        ;;
    --run)
        # Hermit gives the guest a fresh isolated /tmp per repeat; create the
        # working directory before the roundtrip write.
        work="${E2E_TMPDIR:-/tmp}/hermit-tcl-rand-clock"
        rm -rf "$work"
        mkdir -p "$work"

        # The Tcl program is fed on stdin so no on-disk script path is needed.
        WORK="$work" exec tclsh <<'TCL'
# PRNG seeded from pid + wall clock: varies natively, determinized by Hermit.
set r {}
for {set i 0} {$i < 4} {incr i} { lappend r [expr {int(rand()*1000000)}] }

# Deterministic-by-construction container ops as a stable cross-check.
set words {gamma alpha delta beta epsilon zeta}
set sorted [lsort $words]
set d [dict create]
set idx 0
foreach w $sorted { dict set d $w [expr {$idx*$idx}]; incr idx }
set fold 0
dict for {k v} $d { foreach c [split $k ""] { set fold [expr {($fold*31 + [scan $c %c]) % 1000000007}] }; set fold [expr {($fold + $v) % 1000000007}] }

# Clock channels: pinned to the virtual epoch under --strict.
set secs [clock seconds]

# File I/O roundtrip through E2E_TMPDIR.
set path [file join $env(WORK) payload.txt]
set payload "[join $sorted ,]\n[join $r ,]\n$fold\n"
set fh [open $path w]; puts -nonewline $fh $payload; close $fh
set fh [open $path r]; set readback [read $fh]; close $fh

puts [format "TCL rand=%s sorted=%s fold=%d secs=%d bytes=%d roundtrip=%d" \
    [join $r ,] [join $sorted ,] $fold $secs [string length $readback] \
    [expr {$readback eq $payload ? 1 : 0}]]
TCL
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
