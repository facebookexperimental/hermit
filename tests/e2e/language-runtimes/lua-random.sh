#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end Lua 5.4 determinism fixture.
#
# Lua 5.4 auto-seeds its `math.random` generator at interpreter startup from
# kernel entropy (getrandom(2)), so `math.random` returns fresh values on every
# native run without an explicit `math.randomseed`. Hermit intercepts and
# determinizes that entropy at the syscall boundary, so under --strict the seed
# -- and therefore the whole `math.random` sequence -- becomes a pure function
# of the deterministic guest state and repeats bitwise across runs. The rest of
# the program is pure Lua computation (an integer sum-of-squares and a string
# sort), which cross-checks that Lua's arithmetic and library semantics are
# preserved under syscall interception. The program is passed via `-e`, keeping
# this a single fast process with no filesystem dependency (Hermit isolates the
# guest /tmp per repeat).
set -euo pipefail

# Prefer the versioned CI binary; fall back to an unversioned Lua 5.4.
lua_bin() {
    if command -v lua5.4 >/dev/null 2>&1; then
        echo lua5.4
    elif command -v lua >/dev/null 2>&1; then
        echo lua
    else
        return 1
    fi
}

prog='local a=math.random(1,1000000)
local b=math.random(1,1000000)
local c=math.random(1,1000000)
local s=0
for i=1,100 do s=s+i*i end
local t={}
for w in string.gmatch("hermit determinism lua","%a+") do t[#t+1]=w end
table.sort(t)
print(string.format("rand=%d,%d,%d sumsq=%d sorted=%s len=%d",
    a,b,c,s,table.concat(t,"-"),#("abcdefghij")))'

case ${1:-} in
    --prepare)
        lua_bin >/dev/null 2>&1 || {
            echo "lua5.4 not found" >&2
            exit 1
        }
        exit 0
        ;;
    --run)
        exec "$(lua_bin)" -e "$prog"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
