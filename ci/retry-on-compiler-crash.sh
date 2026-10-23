#!/bin/bash
# Run a compiling command, and rerun it ONCE if the compiler itself crashed.
#
# usage: ci/retry-on-compiler-crash.sh COMMAND [ARGS...]
#        ci/retry-on-compiler-crash.sh --self-test
#
# Only `cargo build|check|clippy|doc|rustc|test` is ever rerun. Any other
# command, `cargo nextest` included, is exec'd unchanged, so a test run is never
# repeated by this script.
#
# A rerun happens only when the first run exits non-zero AND its output carries
# one of the two compiler-crash texts below. A real build error, a test
# failure, a timeout or a signal from outside is never rerun, and neither is a
# second crash: the rerun's exit status is the command's. Both texts come from
# the compiler, never from the code it compiles:
#   rustc: "error: rustc interrupted by SIGSEGV" (rustc's own SIGSEGV handler)
#   gcc/g++ (a C or C++ build script): "internal compiler error"
# Why: the full validations of 2026-10-16 died twice on rustc SIGSEGVs in LLVM's
# optimization threads (near-null fault addresses, not stack exhaustion), each
# in a different crate, and neither reproduced when the crate was rebuilt;
# e9tool's g++ crashed the same way at 4:05 AM.
#
# A rerun is never silent:
#   - before the rerun it prints "retry-on-compiler-crash: INFRA RETRY ..." with
#     the command and the crash lines from the first run;
#   - when VALIDATE_RUN_STATE is set (every validation node; the pinned root
#     mounts it at /validate-run-state), it writes the same record to
#     $VALIDATE_RUN_STATE/compiler-crash-retries/, which scripts/validate.rs
#     prints in the run's report, whether the rerun passed or failed;
#   - after the rerun it prints the outcome as the last line, so the node's
#     one-line summary names it when this is the node's last command.

set -uo pipefail

CRASH_PATTERN='error: rustc interrupted by SIGSEGV|internal compiler error'

retries_eligible() { # retries_eligible COMMAND [ARGS...]: whether this command may be rerun
    [[ $# -ge 2 && ${1##*/} == cargo ]] || return 1
    shift
    local arg
    for arg in "$@"; do
        case $arg in
            +*) continue ;; # a toolchain override (cargo +nightly ...)
            -*) return 1 ;; # a global option before the subcommand: not ours to judge
            build | check | clippy | doc | rustc | test) return 0 ;;
            *) return 1 ;;
        esac
    done
    return 1
}

run_logged() { # run_logged LOG COMMAND...: run COMMAND, copying both streams to LOG; return its status
    local log=$1
    shift
    "$@" > >(tee -a -- "$log") 2> >(tee -a -- "$log" >&2)
    local status=$?
    wait
    return "$status"
}

record() { # record TEXT...: print to stderr and keep in the run's state, if any
    printf '%s\n' "$@" >&2
    [[ -n ${VALIDATE_RUN_STATE:-} ]] || return 0
    local dir=$VALIDATE_RUN_STATE/compiler-crash-retries
    mkdir -p -- "$dir" 2>/dev/null || return 0
    # One file per rerun: every record call of this run appends to the same one.
    : "${record_file:=$dir/$(date -u +%Y%m%dT%H%M%S)-$$.txt}"
    printf '%s\n' "$@" >>"$record_file" 2>/dev/null || true
}

main() {
    if ! retries_eligible "$@"; then
        exec "$@"
    fi
    local log status crash_lines
    log=$(mktemp "${TMPDIR:-/tmp}/retry-on-compiler-crash.XXXXXX") || exec "$@"
    run_logged "$log" "$@"
    status=$?
    if ((status == 0)) || ! grep -Eq -- "$CRASH_PATTERN" "$log"; then
        rm -f -- "$log"
        return "$status"
    fi
    crash_lines=$(grep -E -m 3 -- "$CRASH_PATTERN" "$log")
    rm -f -- "$log"
    record "retry-on-compiler-crash: INFRA RETRY 1 of 1: the compiler crashed (exit $status) in: $*" \
        "retry-on-compiler-crash: first run's crash lines:" "$crash_lines" \
        "retry-on-compiler-crash: rerunning once; a second failure of any kind fails the node"
    "$@"
    status=$?
    if ((status == 0)); then
        record "retry-on-compiler-crash: INFRA RETRY: passed on its one rerun after a compiler crash ($(head -1 <<<"$crash_lines")) in: $*"
    else
        record "retry-on-compiler-crash: INFRA RETRY FAILED: the rerun exited $status too, after a compiler crash, in: $*"
    fi
    return "$status"
}

self_test() {
    local dir status out
    dir=$(mktemp -d "${TMPDIR:-/tmp}/retry-on-compiler-crash-test.XXXXXX") || return 2
    trap 'rm -rf -- "$dir"' RETURN
    mkdir -p "$dir/bin" "$dir/state"
    # A fake cargo: crashes while $dir/crashes-left > 0, else prints ok; "fail" always fails plainly.
    cat >"$dir/bin/cargo" <<'EOF'
#!/usr/bin/env bash
left=$(cat "$FAKE_DIR/crashes-left" 2>/dev/null || echo 0)
echo "$@" >>"$FAKE_DIR/calls"
if [[ ${2:-} == fail ]]; then echo "error[E0308]: mismatched types" >&2; exit 101; fi
if ((left > 0)); then
    echo $((left - 1)) >"$FAKE_DIR/crashes-left"
    if [[ ${2:-} == gcc ]]; then echo "foo.cc:1: internal compiler error: Segmentation fault" >&2; else echo "error: rustc interrupted by SIGSEGV, printing backtrace" >&2; fi
    exit 101
fi
echo "Finished"
EOF
    chmod +x "$dir/bin/cargo"
    local self
    self=$(realpath -- "${BASH_SOURCE[0]}")
    check() { # check NAME EXPECTED_STATUS EXPECTED_CALLS CRASHES ARGS...
        local name=$1 want=$2 calls=$3 crashes=$4
        shift 4
        echo "$crashes" >"$dir/crashes-left"
        rm -f "$dir/calls" "$dir/state/compiler-crash-retries/"*
        out=$(FAKE_DIR=$dir VALIDATE_RUN_STATE=$dir/state "$self" "$dir/bin/cargo" "$@" 2>&1)
        status=$?
        local got_calls
        got_calls=$(wc -l <"$dir/calls" 2>/dev/null || echo 0)
        if [[ $status != "$want" || $got_calls != "$calls" ]]; then
            echo "retry-on-compiler-crash self-test FAILED: $name: status $status (want $want), calls $got_calls (want $calls)" >&2
            echo "$out" >&2
            return 1
        fi
    }
    check "clean build runs once" 0 1 0 build || return 1
    check "a plain build error is not rerun" 101 1 0 build fail || return 1
    check "a rustc crash is rerun once and passes" 0 2 1 build || return 1
    grep -q 'INFRA RETRY: passed on its one rerun' <<<"$out" || { echo "self-test: no pass record" >&2; return 1; }
    grep -q 'interrupted by SIGSEGV' "$dir/state/compiler-crash-retries/"* || { echo "self-test: no state record" >&2; return 1; }
    [[ $(find "$dir/state/compiler-crash-retries" -name '*.txt' | wc -l) == 1 ]] || { echo "self-test: one rerun left more than one record" >&2; return 1; }
    check "a g++ crash is rerun once" 0 2 1 build gcc || return 1
    check "a second crash fails the node" 101 2 2 build || return 1
    grep -q 'INFRA RETRY FAILED' <<<"$out" || { echo "self-test: no failure record" >&2; return 1; }
    check "a test run is never rerun" 101 1 1 nextest run || return 1
    check "an unknown subcommand is never rerun" 101 1 1 fmt || return 1
    echo "retry-on-compiler-crash: self-test PASS (clean, plain error, rustc crash, g++ crash, second crash, nextest and other commands never rerun)"
}

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit $?
fi
[[ $# -ge 1 ]] || {
    echo "usage: ci/retry-on-compiler-crash.sh COMMAND [ARGS...] | --self-test" >&2
    exit 2
}
main "$@"
exit $?
