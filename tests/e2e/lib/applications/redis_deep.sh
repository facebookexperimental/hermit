#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Deep Redis application test: a full redis-server + redis-cli session over a
# private AF_UNIX socket, exercising the event loop, networking, virtualized
# time (EXPIRE/TTL), the dict hash seed (SCAN iteration order), embedded Lua
# (EVAL), and the getrandom-seeded server run_id. Natively the run_id (and TTL
# skew) make the session nondeterministic; under Hermit strict mode the whole
# session is bitwise reproducible (L2), so redis's determinism is verified
# end to end rather than command by command.

set -euo pipefail
export LC_ALL=C
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export TZ=UTC

function run_redis_workload {
    local work_dir=$1
    local sock server_pid='' reply session run_id session_hash

    rm -rf -- "$work_dir"
    mkdir -p -- "$work_dir"
    sock="$work_dir/redis.sock"
    session="$work_dir/session.txt"

    trap 'if [[ -n ${server_pid:-} ]]; then \
        redis-cli -s "$sock" shutdown nosave >/dev/null 2>&1 || \
            kill "$server_pid" 2>/dev/null || true; \
        wait "$server_pid" 2>/dev/null || true; fi' RETURN

    # TCP disabled (--port 0); communicate only over the private unix socket so
    # the test never contends for a host TCP port and stays self-contained.
    redis-server \
        --port 0 \
        --unixsocket "$sock" \
        --dir "$work_dir" \
        --save '' \
        --appendonly no \
        --daemonize no \
        --logfile "$work_dir/redis.log" \
        --loglevel warning &
    server_pid=$!

    # Wait for the socket to accept PING (bounded); fail fast if the server dies.
    reply=''
    for _ in $(seq 1 500); do
        if ! kill -0 "$server_pid" 2>/dev/null; then
            cat "$work_dir/redis.log" >&2 2>/dev/null || true
            return 1
        fi
        if [[ -S $sock ]]; then
            reply=$(redis-cli -s "$sock" ping 2>/dev/null || true)
            [[ $reply == PONG ]] && break
        fi
        sleep 0.01
    done
    [[ $reply == PONG ]] || { cat "$work_dir/redis.log" >&2 2>/dev/null || true; return 1; }

    # Data-structure battery. Piped through a SINGLE redis-cli connection (one
    # process, not one per command) so the workload stays well within the shared
    # application-test timeout when run twice under Hermit --verify on a slow
    # portable CI runner. Every reply is captured into the transcript and must be
    # byte-identical across Hermit's two runs. SMEMBERS/SCAN are left in raw
    # server order: the keyspace dict hash seed comes from getrandom, so their
    # order varies natively but is reproduced exactly under Hermit -- that is the
    # determinism stressor, not something to canonicalize away.
    redis-cli -s "$sock" >"$session" <<'CMDS'
set greeting "hello world"
append greeting "!"
get greeting
strlen greeting
incr counter
incr counter
incr counter
incr counter
incr counter
incrby counter 100
get counter
setbit flags 7 1
setbit flags 42 1
bitcount flags
rpush queue a b c d e
lrange queue 0 -1
lpop queue
hset profile name deterministic version 2 mode strict
hgetall profile
sadd tags red green blue red
scard tags
smembers tags
zadd scores 3 gamma 1 alpha 2 beta
zrange scores 0 -1 withscores
setex ephemeral 100 present
ttl ephemeral
persist ephemeral
ttl ephemeral
eval "return redis.call('get', KEYS[1]) .. ':' .. redis.call('get', KEYS[2])" 2 greeting counter
scan 0 count 1000
eval "redis.call('set', 'tx', 'committed'); return redis.call('get', 'tx')" 0
dbsize
CMDS

    # Entropy witness: server run_id is a 40-hex getrandom value. Determinized
    # under Hermit, random natively -> drives assert_native_nondeterminism.
    run_id=$(redis-cli -s "$sock" info server | tr -d '\r' \
        | sed -n 's/^run_id://p')
    [[ ${#run_id} -eq 40 ]]

    redis-cli -s "$sock" shutdown nosave >/dev/null 2>&1 || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=

    # Sanity: transcript is well-formed regardless of determinism.
    [[ $(sed -n '1p' "$session") == OK ]]
    grep -Fxq "hello world!" "$session"

    session_hash=$(sha256sum "$session" | cut -d' ' -f1)
    printf 'redis-deep:%s:%s\n' "$run_id" "$session_hash"
}

if [[ ${1:-} == --guest ]]; then
    run_redis_workload "$2"
    exit
fi

# shellcheck source=tests/e2e/lib/applications/common.sh
source "$(dirname -- "$0")/common.sh"
require_commands redis-server redis-cli sed seq sha256sum tr timeout

work_root=$(mktemp -d "${TMPDIR:-/tmp}/hermit-redis-e2e.XXXXXX")
trap 'rm -rf -- "$work_root"' EXIT

native_first=$(run_redis_workload "$work_root/native")
native_second=$(run_redis_workload "$work_root/native")
assert_native_nondeterminism 'Redis deep workload' "$native_first" "$native_second"

run_hermit_verify 'Redis deep workload' \
    /bin/bash "$0" --guest "$work_root/verified" >/dev/null
printf 'redis-deep:verified\n'
