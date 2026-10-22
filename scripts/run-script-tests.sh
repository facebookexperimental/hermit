#!/usr/bin/env bash
#
# Run the unit tests carried by rust-script entrypoints.
#
# rust-scripts are not Cargo workspace members, so `cargo test` never sees them.
# `build.rust_scripts` compiles all tracked entrypoints and their test harnesses
# before this graph consumer runs. This script executes those harnesses; outside
# the graph, rust-script retains its ordinary compile-and-run behavior. Measured
# 2026-08-25 at hermit main a5fef7ff7623: seven entrypoints carried 84 unit tests
# between them and no target anywhere passed `--test`, so all 84 were documentation.
# They all passed once run, so this gate starts green.
#
# The file list is DISCOVERED, not hard-coded: a new script that grows a test
# module is picked up with no edit here. A hard-coded list would reproduce exactly
# the "someone has to remember" failure this gate was added to close.
#
# The harnesses are independent processes, so up to HERMIT_SCRIPT_TEST_JOBS of
# them run at once (the CI node check.script_unit_tests sets it to its admitted
# width; otherwise every online CPU). Each harness's stdout and stderr are
# buffered separately and printed whole, to the same streams, as soon as that
# harness finishes, so a node stopped by its wall bound still shows every
# harness that completed. Measured serially at d44bbbb79acd, unthrottled: 362 s,
# of which ci/compat-envelope/scorecard.rs took 162 s and scripts/validate.rs
# 131 s.
#
# A harness named in `shards` below runs as that many processes instead of one.
# Each process runs a disjoint share of the harness's own test list, selected by
# exact name, and must report every test of its share as passed or ignored and
# every other test as filtered out, so the shares together run exactly the
# list. Sharding changes only the process a test runs in, never which tests run.
# The table is a scheduling hint, not a test list: a harness left out of it
# still runs whole, and an entry that names no discovered harness is reported
# and ignored (a fixture checkout with other harnesses, or a renamed file).
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v rust-script >/dev/null 2>&1; then
    echo 'error: rust-script is not installed (cargo install rust-script)' >&2
    exit 1
fi

jobs=${HERMIT_SCRIPT_TEST_JOBS:-$(nproc)}
if ! [[ $jobs =~ ^[1-9][0-9]*$ ]]; then
    echo "error: HERMIT_SCRIPT_TEST_JOBS must be a positive integer, got '${jobs}'" >&2
    exit 2
fi

# ci/compat-envelope/scorecard.rs: 49 of its 165 tests change the process's
# environment or working directory, so they hold one process-wide lock
# (HISTORY_FIXTURE_LOCK) and run one at a time whatever the thread count. Run
# alone, serially, 47 of them took 304 s at a25b3fcd3805, so that lock bounded
# the harness, and with it check.script_unit_tests, at about 330 s on 8 cores.
# The lock guards no data, only that process-wide state, so separate processes
# need no ordering between them. Tests are dealt round-robin in list order, which
# spreads the locked tests (they share module prefixes) evenly across shards.
declare -A shards=(
    [ci/compat-envelope/scorecard.rs]=4
)

files=()
for f in $(git ls-files '*.rs' ':!:third-party/**' ':!:scripts/lib/**'); do
    # Only standalone entrypoints: a rust-script shebang makes the file runnable
    # on its own, which is what `rust-script --test` requires.
    head -n 1 -- "$f" | grep -q 'rust-script' || continue
    grep -q '#\[cfg(test)\]' -- "$f" || continue
    files+=("$f")
done

total=${#files[@]}
if [ "$total" -eq 0 ]; then
    # Not a pass. Discovery returning nothing means the shebang or the test-module
    # spelling moved and this gate is now measuring an empty set -- the silent-green
    # shape. Fail loudly instead.
    echo 'error: run-script-tests discovered no test-carrying rust-scripts; expected at least one' >&2
    exit 1
fi

for sharded in "${!shards[@]}"; do
    found=false
    for f in "${files[@]}"; do
        [ "$f" = "$sharded" ] && found=true
    done
    if ! "$found"; then
        echo "run-script-tests: note: shards names ${sharded}, which is not a discovered test-carrying rust-script; ignored" >&2
        unset 'shards[$sharded]'
        continue
    fi
    if ! [[ ${shards[$sharded]} =~ ^[1-9][0-9]*$ ]]; then
        echo "error: run-script-tests shard count for ${sharded} must be a positive integer" >&2
        exit 2
    fi
done

# Arguments meant for the test harness itself. The prepared runner in
# ci/rust-script-bin hands everything after the source to the harness. The
# ordinary rust-script runs `cargo test` with them, and cargo keeps for itself
# whatever precedes a `--`, passing on to the harness only what follows it.
harness_args=(--)
runner=$(command -v rust-script)
if [ "$(realpath -- "$runner")" = "$(pwd -P)/ci/rust-script-bin/rust-script" ]; then
    harness_args=()
fi

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

# A unit is one harness process: a whole harness, or one shard of a sharded one.
unit_file=()
unit_label=()
unit_names=()
unit_listed=()
failed_listing=()
for f in "${files[@]}"; do
    n=${shards[$f]:-1}
    if [ "$n" -eq 1 ]; then
        unit_file+=("$f")
        unit_label+=("$f")
        unit_names+=('')
        unit_listed+=(0)
        continue
    fi
    list=$out/list.${#unit_file[@]}
    status=0
    ./ci/rust-script-bin/run-test-harness "$f" -- rust-script --test "$f" \
        "${harness_args[@]}" --list --format terse >"$list.stdout" 2>"$list.stderr" || status=$?
    names=()
    if [ "$status" = 0 ]; then
        while IFS= read -r line; do
            [ -n "$line" ] || continue
            # libtest's terse list prints exactly `NAME: test` or `NAME: bench`.
            # Anything else means this parse no longer describes the list.
            if [[ $line =~ ^([^[:space:]]+):\ (test|bench)$ ]]; then
                names+=("${BASH_REMATCH[1]}")
            else
                printf 'run-script-tests: unexpected test-list line from %s: %s\n' "$f" "$line" >"$list.why"
                status=list
                break
            fi
        done <"$list.stdout"
    fi
    if [ "$status" = 0 ] && [ "${#names[@]}" -eq 0 ]; then
        # The same silent-green shape as an empty discovery.
        printf 'run-script-tests: %s listed no tests\n' "$f" >"$list.why"
        status=list
    fi
    if [ "$status" != 0 ]; then
        printf 'run-script-tests: %s (listing tests)\n' "$f"
        cat -- "$list.stdout"
        cat -- "$list.stderr" >&2
        [ -e "$list.why" ] && cat -- "$list.why" >&2
        echo "run-script-tests: FAILED ${f} (cannot list its tests for sharding, status ${status})" >&2
        failed_listing+=("$f")
        continue
    fi
    count=${#names[@]}
    [ "$n" -le "$count" ] || n=$count
    for ((k = 0; k < n; k++)); do
        names_file=$out/names.${#unit_file[@]}
        : >"$names_file"
        for ((i = k; i < count; i += n)); do
            printf '%s\n' "${names[$i]}" >>"$names_file"
        done
        unit_file+=("$f")
        unit_label+=("$f (shard $((k + 1)) of ${n}: $(wc -l <"$names_file") of ${count} tests)")
        unit_names+=("$names_file")
        unit_listed+=("$count")
    done
done

libtest_summary='^test result: ok\. ([0-9]+) passed; 0 failed; ([0-9]+) ignored; ([0-9]+) measured; ([0-9]+) filtered out;'
run_one() {
    local index=$1 status=0 file=${unit_file[$1]} names_file=${unit_names[$1]}
    local args=() names=() summary
    if [ -n "$names_file" ]; then
        mapfile -t names <"$names_file"
        args=("${harness_args[@]}" --exact "${names[@]}")
    fi
    # Also cover ordinary rust-script outside the prepared validation graph.
    # The prepared dispatcher adds a nested scope of its own; both directories
    # are retained, and each harness starts with the original outer root.
    ./ci/rust-script-bin/run-test-harness "$file" -- rust-script --test "$file" "${args[@]}" \
        >"$out/$index.stdout" 2>"$out/$index.stderr" || status=$?
    if [ -n "$names_file" ] && [ "$status" = 0 ]; then
        # A shard passes only if it ran exactly its share: every assigned test
        # passed or was ignored, and every other listed test was filtered out.
        summary=$(grep -E '^test result: ' "$out/$index.stdout" | tail -n 1 || true)
        if [[ $summary =~ $libtest_summary ]] &&
            [ $((BASH_REMATCH[1] + BASH_REMATCH[2] + BASH_REMATCH[3])) -eq "${#names[@]}" ] &&
            [ "${BASH_REMATCH[4]}" -eq $((unit_listed[index] - ${#names[@]})) ]; then
            :
        else
            printf 'run-script-tests: shard ran a different set than its %s assigned of %s listed tests: %s\n' \
                "${#names[@]}" "${unit_listed[$index]}" "${summary:-no libtest summary}" >>"$out/$index.stderr"
            status=count
        fi
    fi
    printf '%s\n' "$status" >"$out/$index.status"
    {
        flock 9
        printf 'run-script-tests: %s\n' "${unit_label[$index]}"
        cat -- "$out/$index.stdout"
        cat -- "$out/$index.stderr" >&2
        if [ "$status" != 0 ]; then
            echo "run-script-tests: FAILED ${unit_label[$index]} (status ${status})" >&2
        fi
    } 9>"$out/print.lock"
}

running=0
for index in "${!unit_file[@]}"; do
    run_one "$index" &
    running=$((running + 1))
    if [ "$running" -ge "$jobs" ]; then
        wait -n
        running=$((running - 1))
    fi
done
wait

declare -A failed_files=()
for f in "${failed_listing[@]}"; do
    failed_files[$f]=1
done
for index in "${!unit_file[@]}"; do
    # A missing status file means the harness wrapper itself did not complete;
    # that is a failure, never a pass.
    status=$(cat -- "$out/$index.status" 2>/dev/null || echo missing)
    if [ "$status" != 0 ]; then
        if [ "$status" = missing ]; then
            echo "run-script-tests: FAILED ${unit_label[$index]} (no status recorded)" >&2
        fi
        failed_files[${unit_file[$index]}]=1
    fi
done

failed=${#failed_files[@]}
if [ "$failed" -ne 0 ]; then
    echo "run-script-tests: ${failed} of ${total} script test suites failed" >&2
    exit 1
fi

printf 'run-script-tests: OK -- %s script test suites passed\n' "$total"
