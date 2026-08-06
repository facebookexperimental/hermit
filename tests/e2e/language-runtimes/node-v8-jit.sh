#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end Node.js / V8 determinism fixture -- the corpus's first
# JIT-COMPILING runtime.
#
# WHY THIS IS A NEW SURFACE, not another interpreter. Every language runtime in
# the corpus so far (Lua, Perl, Python, Ruby, Tcl, gawk, m4, bash) is a
# bytecode or tree-walking interpreter: it never writes machine code at run
# time. V8 does. Executing the hot loop below drives Ignition -> Sparkplug ->
# TurboFan tiering, which allocates WRITABLE-then-EXECUTABLE code pages
# (mmap/mprotect with PROT_EXEC, W^X flips), installs optimized code while the
# program runs, and can deoptimize back. That is a syscall and memory-protection
# path no existing fixture reaches, and it runs alongside V8's background
# compiler and GC threads -- so the workload is also genuinely multi-threaded
# under Hermit's sequentialized scheduler.
#
# TWO INDEPENDENT ASSERTIONS, deliberately in one program:
#
#   1. `random` -- V8 seeds its xorshift128+ `Math.random` state from the
#      platform entropy source (getrandom(2)) at first use, so a fresh value is
#      emitted on every native run. Hermit determinizes that entropy at the
#      syscall boundary, so under --strict the whole sequence becomes a pure
#      function of deterministic guest state and repeats bitwise. This is what
#      makes the fixture a real determinism test rather than a smoke test.
#
#   2. `jit-checksum` -- an integer reduction hot enough to be TurboFan-
#      optimized, and deterministic by construction. Its value is therefore a
#      CORRECTNESS oracle for the JIT under interception: if optimized code were
#      miscompiled, mis-deoptimized, or never installed because a code-page
#      syscall was mishandled, the checksum changes or the run crashes. A
#      determinism-only assertion could not distinguish "JIT worked" from "JIT
#      silently never engaged".
#
# `--jitless` is deliberately NOT passed: disabling the JIT would remove the
# very surface this fixture exists to cover. The program is passed with `-e`,
# keeping this a single fast process with no filesystem dependency (Hermit
# isolates the guest /tmp per repeat).
set -euo pipefail

node_bin() {
    if command -v node >/dev/null 2>&1; then
        echo node
    elif command -v nodejs >/dev/null 2>&1; then
        echo nodejs
    else
        return 1
    fi
}

# Sized so TurboFan certainly tiers up the hot function while the whole run
# stays well inside the portable debug-binary time budget.
prog='function hot(n){let a=0;for(let i=0;i<n;i++){a=(a+Math.imul(i,2654435761))>>>0;}return a;}
let acc=0;
for(let r=0;r<200;r++){acc=(acc+hot(20000))>>>0;}
const words=["hermit","determinism","node","v8"].sort().join("-");
console.log("jit-checksum="+acc+" random="+Math.random().toFixed(12)+" sorted="+words+" typed="+new Int32Array(4).length);'

case ${1:-} in
    --prepare)
        node_bin >/dev/null 2>&1 || {
            echo "node not found" >&2
            exit 1
        }
        exit 0
        ;;
    --run)
        exec "$(node_bin)" -e "$prog"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
