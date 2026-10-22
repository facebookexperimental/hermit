#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# End-to-end jq (JSON processor) determinism fixture.
#
# jq's `now` builtin reads the wall clock (gettimeofday) and returns fractional
# seconds, so it varies every run natively. The rest of the program is a pure,
# deterministic JSON transform -- sorting, grouping, arithmetic, and string
# length over a fixed document -- plus a file-I/O roundtrip through E2E_TMPDIR.
# Under Hermit --strict the clock is pinned to the virtual epoch, so `now` is
# identical across runs and the whole pipeline emits bitwise-identical JSON. The
# transform results are deterministic by construction and cross-check that jq's
# value semantics are preserved under syscall interception.
set -euo pipefail

case ${1:-} in
    --prepare)
        command -v jq >/dev/null 2>&1 || {
            echo "jq not found" >&2
            exit 1
        }
        exit 0
        ;;
    --run)
        # Hermit gives the guest a fresh isolated /tmp per repeat; create the
        # working directory before the roundtrip write.
        work="${E2E_TMPDIR:-/tmp}/hermit-jq-json-transform"
        rm -rf "$work"
        mkdir -p "$work"
        doc="${work}/doc.json"

        # A fixed input document written into the guest-visible working dir.
        cat >"$doc" <<'JSON'
{"items":[{"k":"gamma","v":3},{"k":"alpha","v":1},{"k":"delta","v":4},{"k":"beta","v":1}],
 "tags":["z","a","m","a","z"]}
JSON

        # Deterministic transform + the `now` clock channel; a single jq process.
        result="$(jq -c '{
            sorted: (.items | sort_by(.k) | map(.k)),
            total: (.items | map(.v) | add),
            uniq_tags: (.tags | unique),
            tag_counts: (.tags | group_by(.) | map({key: .[0], n: length})),
            t: now
        }' "$doc")"

        # File I/O roundtrip through E2E_TMPDIR.
        out="${work}/out.json"
        printf '%s\n' "$result" >"$out"
        readback="$(cat "$out")"
        [ "$readback" = "$result" ] && rt=1 || rt=0

        printf 'JQ %s bytes=%d roundtrip=%d\n' "$result" "${#readback}" "$rt"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
