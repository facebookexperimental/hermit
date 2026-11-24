#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# In-guest driver for the original flaky server: starts the
# UNMODIFIED external_flaky_service/flaky_server.sh, makes the
# requested number of loopback requests in order, prints the reply
# sequence, and cleans up exactly the processes it started. It exits
# 0 only when every reply arrived as a single 0 or 1 (the protocol
# property) and cleanup is verified; any other outcome - a lost or
# malformed reply, a server that will not start or will not die -
# fails by name below. The bits themselves are bash RANDOM draws:
# seeded by the guest pid and virtual time, not by Hermit's --seed,
# so no golden sequence is asserted anywhere.
#
# Usage: flaky_server_guest.sh <flaky_server.sh> <requests> <port>

set -u

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <flaky_server.sh> <requests> <port>" >&2
    exit 2
fi

server="$1"
requests="$2"
port="$3"
case "$requests" in
    ''|*[!0-9]*|0)
        echo "FAIL: requests must be a positive integer, got '$requests'" >&2
        exit 2
        ;;
esac
case "$port" in
    ''|*[!0-9]*|0)
        echo "FAIL: port must be a positive integer, got '$port'" >&2
        exit 2
        ;;
esac
if [ "$port" -gt 65535 ]; then
    echo "FAIL: port must be at most 65535, got '$port'" >&2
    exit 2
fi
if [ ! -x "$server" ]; then
    echo "FAIL: flaky server script is not executable: $server" >&2
    exit 1
fi
if ! command -v pgrep >/dev/null 2>&1; then
    echo "FAIL: pgrep not found; cannot record server children for cleanup" >&2
    exit 1
fi
if ! command -v nc >/dev/null 2>&1; then
    echo "FAIL: nc not found in guest; cannot make requests" >&2
    exit 1
fi

# Exact cleanup: only pids recorded below (the server script and
# every child it had when recorded - its request-handler coproc and
# its nc listener). Checking their death is part of the guest's
# verdict, not best effort: a surviving listener would hold
# Hermit's whole run open.
server_pid=""
listener_pids=()
# kill -0 succeeds on zombies, which are dead for cleanup purposes:
# only a live (non-Z) /proc state counts as still alive. Parsed with
# bash builtins only: the comm in parentheses may itself contain
# spaces or parentheses, so strip through the LAST ") " and take the
# first field after it (the state). The redirect's own error is
# silenced, so checking a reaped pid (whose stat file is gone) does
# not print into guest stderr; a failed read still counts as dead,
# while an empty or malformed state counts as alive (fail safe: only
# a parsed Z, or a vanished stat file, is proof of death).
pid_alive() {
    local line state
    { read -r line < "/proc/$1/stat"; } 2>/dev/null || return 1
    line="${line##*) }"
    state="${line%% *}"
    [ -z "$state" ] && return 0
    [ "$state" != Z ]
}

cleanup() {
    local pid
    for pid in "${listener_pids[@]}" "$server_pid"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null
    done
    [ -n "$server_pid" ] && wait "$server_pid" 2>/dev/null
    for pid in "${listener_pids[@]}" "$server_pid"; do
        if [ -n "$pid" ] && pid_alive "$pid"; then
            kill -9 "$pid" 2>/dev/null
        fi
    done
    for pid in "${listener_pids[@]}" "$server_pid"; do
        if [ -n "$pid" ] && pid_alive "$pid"; then
            echo "FAIL: cleanup left started process $pid alive" >&2
            return 1
        fi
    done
    return 0
}

"$server" "$requests" "$port" &
server_pid=$!

# The original has no readiness signal. Poll, with a limit, until the
# request-handler coproc and an nc child both exist, then record every
# child the server script has (discovered before any kill; never a
# broad pattern kill). Recording only the first child, or recording
# before a late nc fork, would leave the listener alive outside the
# record and Hermit waiting on it.
poll_attempt=0
while [ "$poll_attempt" -lt 50 ]; do
    if [ -n "$(pgrep -P "$server_pid" -x nc)" ] \
            && [ "$(pgrep -P "$server_pid" | wc -l)" -ge 2 ]; then
        break
    fi
    sleep 0.2
    poll_attempt=$((poll_attempt + 1))
done
mapfile -t listener_pids < <(pgrep -P "$server_pid")
if [ "${#listener_pids[@]}" -lt 2 ] || [ -z "$(pgrep -P "$server_pid" -x nc)" ]; then
    echo "FAIL: server $server_pid children not ready to record for cleanup after polling (children: ${listener_pids[*]:-none})" >&2
    cleanup || true
    exit 1
fi

sequence=""
i=0
while [ "$i" -lt "$requests" ]; do
    # A request sent before the server's coproc is reading can be
    # lost; resend a bounded number of times for this slot. Holding
    # stdin open briefly lets nc wait for the reply before closing.
    out=""
    attempt=0
    while [ -z "$out" ] && [ "$attempt" -lt 3 ]; do
        out="$( (printf ' \n'; sleep 1) | nc -w 3 localhost "$port")"
        attempt=$((attempt + 1))
    done
    sequence="$sequence$out"
    i=$((i + 1))
done

echo "sequence=$sequence"
if ! cleanup; then
    exit 1
fi
case "$sequence" in
    *[!01]*|"")
        echo "FAIL: incomplete or non-bit sequence" >&2
        exit 1
        ;;
esac
if [ "${#sequence}" -ne "$requests" ]; then
    echo "FAIL: sequence length ${#sequence} != requests $requests" >&2
    exit 1
fi
echo "guest: $requests replies in order, each 0/1"
exit 0
