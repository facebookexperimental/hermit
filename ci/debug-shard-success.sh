#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Debug shard success records for .github/workflows/ci-portable.yml.
#
# The strict-compat job runs compatprep.fixtures_on_host, whose constructed
# predecessors include nodes of some test-debug matrix legs (unit,
# unit-parallel, strict-verify, clippy and docs today), so it needs test-debug.
# GitHub reports one result for a whole matrix job, though, so with a plain
# `needs` the failure of any leg, also one that runs none of those
# predecessors, skipped strict-compat. Its parity archive was then missing and
# the reducer failed with "expected 13 parity archives, found 12" instead of
# reporting the leg that failed (runs 38106386550 and 38110887257: the
# integration leg failed). Each leg now records its success, and strict-compat
# requires exactly the legs that own a node in its dependency closure.
#
#   debug-shard-success.sh record SLUG NODES
#       In a test-debug leg, after its nodes passed: write
#       ignored/debug-shard-ok/SLUG.json naming the leg, its nodes, the commit
#       and the run.
#   debug-shard-success.sh require DIR NODES
#       In strict-compat: refuse unless every debug shard of
#       ci/portable-shards.json that owns a node in the transitive dependency
#       closure of NODES (comma-separated) in ci/dag/validate.json left a
#       record in DIR for this commit and run, naming its nodes as the shard
#       map does.
#   debug-shard-success.sh self-test
#       Check both modes against fixtures.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
SHARDS=${DEBUG_SHARD_SUCCESS_SHARDS:-$ROOT_DIR/ci/portable-shards.json}
DAG=${DEBUG_SHARD_SUCCESS_DAG:-$ROOT_DIR/ci/dag/validate.json}
# The hosted workflow runs the steps of this label, and the shard map names
# them as validate's hosted selector resolves them (check-shard-coverage.sh).
LABEL=hosted-portable

die() {
    echo "debug-shard-success.sh: $*" >&2
    exit 1
}

run_key() {
    [[ -n ${GITHUB_SHA:-} && -n ${GITHUB_RUN_ID:-} && -n ${GITHUB_RUN_ATTEMPT:-} ]] ||
        die "GITHUB_SHA, GITHUB_RUN_ID and GITHUB_RUN_ATTEMPT must be set"
    echo "$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT"
}

record() {
    local slug=$1 nodes=$2 key out
    [[ $slug =~ ^[a-z0-9][a-z0-9-]*$ ]] || die "bad shard slug: '$slug'"
    key=$(run_key)
    jq -e --arg slug "$slug" --arg nodes "$nodes" '
        [.debug_shards[] | select(.slug == $slug) | .nodes | join(",")] == [$nodes]
    ' "$SHARDS" >/dev/null || die "shard $slug with nodes '$nodes' is not in $SHARDS"
    out="$ROOT_DIR/ignored/debug-shard-ok/$slug.json"
    mkdir -p "$(dirname "$out")"
    jq -n --arg slug "$slug" --arg nodes "$nodes" --arg sha "$GITHUB_SHA" --arg run_key "$key" \
        '{schema: 1, slug: $slug, nodes: ($nodes | split(",")),
          repository_sha: $sha, run_key: $run_key}' >"$out"
    echo "debug-shard-success.sh: recorded success of debug shard $slug" >&2
}

# The debug shards owning a node in the dependency closure of NODES among the
# steps labelled LABEL, one per line as "slug<TAB>node,node". Shard map names
# resolve as validate's hosted selector does: the exact name of a LABEL step,
# else the name with _on_host appended. The full DAG also has a step under the
# short name (test.hermit_unit beside test.hermit_unit_on_host), so resolving
# against every step would find no shard at all.
required_shards() {
    local nodes=$1
    jq -rn --arg roots "$nodes" --arg step_label "$LABEL" --slurpfile dag "$DAG" \
        --slurpfile shards "$SHARDS" '
        ($dag[0].steps | map(select(.labels | index($step_label)))
         | map({key: "\(.group).\(.job)", value: (.deps // [])})
         | from_entries) as $deps
        | def resolve: if $deps[.] == null and $deps[. + "_on_host"] != null
                       then . + "_on_host" else . end;
        ($roots | split(",") | map(resolve)) as $start
        | (if ($start | map(select($deps[.] == null)) | length) > 0
           then error("not in the DAG: \($start | map(select($deps[.] == null)) | join(", "))")
           else . end)
        | {todo: $start, seen: []}
        | until(.todo | length == 0;
            .todo[0] as $n | .todo |= .[1:]
            | if (.seen | index($n)) != null then .
              else .seen += [$n] | .todo += ($deps[$n] // []) end)
        | .seen as $closure
        | $shards[0].debug_shards[]
        | [.nodes[] | resolve | select(. as $n | $closure | index($n))] as $owned
        | select($owned | length > 0)
        | "\(.slug)\t\($owned | join(","))"
    '
}

require() {
    local dir=$1 nodes=$2 key slug owned file missing=0 required
    key=$(run_key)
    required=$(required_shards "$nodes") || die "cannot resolve the dependency closure of $nodes"
    [[ -n $required ]] || {
        echo "debug-shard-success.sh: $nodes depend on no debug shard" >&2
        return 0
    }
    while IFS=$'\t' read -r slug owned; do
        file="$dir/$slug.json"
        if [[ ! -f $file ]]; then
            echo "debug-shard-success.sh: debug shard $slug did not succeed (no success record); $nodes depend on its $owned" >&2
            missing=$((missing + 1))
        elif ! jq -e --slurpfile shards "$SHARDS" --arg slug "$slug" --arg sha "$GITHUB_SHA" \
            --arg run_key "$key" '
                .schema == 1 and .slug == $slug and .repository_sha == $sha
                and .run_key == $run_key
                and .nodes == [$shards[0].debug_shards[] | select(.slug == $slug) | .nodes[]]
            ' "$file" >/dev/null; then
            echo "debug-shard-success.sh: the success record of debug shard $slug is not this run's: $(jq -c . "$file" 2>/dev/null || echo unreadable)" >&2
            missing=$((missing + 1))
        else
            echo "debug-shard-success.sh: debug shard $slug succeeded ($owned)" >&2
        fi
    done <<<"$required"
    ((missing == 0)) || die "$missing debug shard(s) that $nodes depend on did not succeed"
}

self_test() {
    local tmp status=0 out
    tmp=$(mktemp -d)
    trap 'rm -rf -- "$tmp"' RETURN
    cat >"$tmp/dag.json" <<'JSON'
{"steps": [
  {"group": "build", "job": "tree", "labels": ["hosted-portable"], "deps": []},
  {"group": "test", "job": "unit", "labels": ["full"], "deps": []},
  {"group": "test", "job": "unit_on_host", "labels": ["hosted-portable"], "deps": ["build.tree"]},
  {"group": "test", "job": "integration_on_host", "labels": ["hosted-portable"], "deps": ["build.tree"]},
  {"group": "lint", "job": "clippy", "labels": ["hosted-portable"], "deps": ["build.tree"]},
  {"group": "prep", "job": "fixtures_on_host", "labels": ["hosted-portable"], "deps": ["test.unit_on_host", "lint.clippy"]},
  {"group": "e2e", "job": "manifest_compat_on_host", "labels": ["hosted-portable"], "deps": ["prep.fixtures_on_host"]}
]}
JSON
    cat >"$tmp/shards.json" <<'JSON'
{"debug_shards": [
  {"slug": "unit", "nodes": ["test.unit"]},
  {"slug": "integration", "nodes": ["test.integration"]},
  {"slug": "clippy", "nodes": ["lint.clippy"]}
]}
JSON
    export DEBUG_SHARD_SUCCESS_DAG=$tmp/dag.json DEBUG_SHARD_SUCCESS_SHARDS=$tmp/shards.json
    export GITHUB_SHA=0123456789abcdef0123456789abcdef01234567 GITHUB_RUN_ID=7 GITHUB_RUN_ATTEMPT=1
    local roots=prep.fixtures_on_host,e2e.manifest_compat_on_host
    expect() { # description, expected status, command...
        local description=$1 want=$2 got=0
        shift 2
        out=$("$@" 2>&1) || got=$?
        if { [[ $want == ok ]] && ((got != 0)); } || { [[ $want == fail ]] && ((got == 0)); }; then
            echo "debug-shard-success.sh self-test: FAIL: $description (status $got): $out" >&2
            status=1
        fi
    }
    out=$(SHARDS=$tmp/shards.json DAG=$tmp/dag.json required_shards "$roots")
    if [[ $out != $'unit\ttest.unit_on_host\nclippy\tlint.clippy' ]]; then
        echo "debug-shard-success.sh self-test: FAIL: closure owners: $out" >&2
        status=1
    fi
    local records=$tmp/records
    mkdir -p "$records"
    rec() { # slug nodes [sha] [run id]
        jq -n --arg slug "$1" --arg nodes "$2" --arg sha "${3:-$GITHUB_SHA}" \
            --arg run_key "${4:-$GITHUB_RUN_ID}-$GITHUB_RUN_ATTEMPT" \
            '{schema: 1, slug: $slug, nodes: ($nodes | split(",")),
              repository_sha: $sha, run_key: $run_key}' >"$records/$1.json"
    }
    # Run 38110887257: only the integration leg failed, so strict-compat runs.
    rec unit test.unit
    rec clippy lint.clippy
    expect "an unrelated failed leg blocks strict-compat" ok \
        bash "${BASH_SOURCE[0]}" require "$records" "$roots"
    rm -f "$records/clippy.json"
    expect "a missing dependency leg is accepted" fail \
        bash "${BASH_SOURCE[0]}" require "$records" "$roots"
    rec clippy lint.clippy 89abcdef0123456789abcdef0123456789abcdef
    expect "another commit's record is accepted" fail \
        bash "${BASH_SOURCE[0]}" require "$records" "$roots"
    rec clippy lint.clippy "$GITHUB_SHA" 6
    expect "another run's record is accepted" fail \
        bash "${BASH_SOURCE[0]}" require "$records" "$roots"
    rec clippy lint.clippy,test.other
    expect "a record naming other nodes is accepted" fail \
        bash "${BASH_SOURCE[0]}" require "$records" "$roots"
    expect "an unknown root is accepted" fail \
        bash "${BASH_SOURCE[0]}" require "$records" "prep.nonexistent_on_host"
    expect "recording a leg the shard map lacks succeeds" fail \
        bash "${BASH_SOURCE[0]}" record unit test.other
    # The committed graph: strict-compat depends on some debug shard, so a
    # resolution that finds none is a defect here, not a pass in CI.
    local real_nodes
    real_nodes=$(jq -r '.strict_compat_nodes | join(",")' "$ROOT_DIR/ci/portable-shards.json")
    out=$(SHARDS=$ROOT_DIR/ci/portable-shards.json DAG=$ROOT_DIR/ci/dag/validate.json \
        required_shards "$real_nodes") || out=""
    if [[ -z $out ]]; then
        echo "debug-shard-success.sh self-test: FAIL: the committed strict_compat_nodes ($real_nodes) resolve to no debug shard" >&2
        status=1
    fi
    ((status == 0)) && echo "debug-shard-success.sh self-test: OK" >&2
    return "$status"
}

case ${1:-} in
record)
    (($# == 3)) || die "usage: record SLUG NODES"
    record "$2" "$3"
    ;;
require)
    (($# == 3)) || die "usage: require DIR NODES"
    require "$2" "$3"
    ;;
self-test)
    (($# == 1)) || die "usage: self-test"
    self_test
    ;;
*)
    die "usage: debug-shard-success.sh record SLUG NODES | require DIR NODES | self-test"
    ;;
esac
