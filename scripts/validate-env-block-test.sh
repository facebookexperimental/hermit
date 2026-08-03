#!/usr/bin/env bash
# Self-test for validate.sh's is_environmental_block() sandbox-failure detector.
#
# The detector separates ENVIRONMENTAL sandbox denials (BpfJailer FS/EXEC/NET
# enforcement leaking into build/test tools) from genuine product/test failures.
# Misclassifying either direction is harmful: a real failure hidden as
# "environmental" silently greens a broken change, and a sandbox denial reported
# as a test failure sends someone to debug a nonexistent product bug.
#
# This test extracts the EXACT shipped regex from validate.sh (the two
# `readonly ENV_BLOCK_*` lines) rather than duplicating it, so the fixtures
# always exercise what ships. It feeds each fixture through the identical
# `sed | grep -qiE` pipeline the detector uses and asserts match / no-match.
#
#     scripts/validate-env-block-test.sh
#
# Exits 0 when every case matches its expectation, 1 otherwise.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly VALIDATE="$SCRIPT_DIR/../validate.sh"

if [[ ! -f "$VALIDATE" ]]; then
    echo "FATAL: cannot find validate.sh at $VALIDATE" >&2
    exit 1
fi

# Pull the single-source-of-truth pattern definitions out of validate.sh and
# evaluate them here. They are plain `readonly NAME='...'` / `readonly NAME="..."`
# lines with no command substitution, so sourcing just those two lines is safe.
pattern_lines=$(grep -E '^readonly ENV_BLOCK_(ERRNOS|PATTERN)=' "$VALIDATE")
if [[ $(grep -c . <<<"$pattern_lines") -ne 2 ]]; then
    echo "FATAL: expected exactly 2 ENV_BLOCK_* readonly lines in validate.sh," \
         "found:" >&2
    echo "$pattern_lines" >&2
    exit 1
fi
# ENV_BLOCK_ERRNOS must be defined before ENV_BLOCK_PATTERN (which references it);
# grep preserves file order, so this is already correct.
eval "$pattern_lines"

# Mirror the exact detector pipeline (see is_environmental_block in validate.sh).
classify() { # stdin = captured log region; returns 0 when environmental
    sed $'s/\033\\[[0-9;]*[[:alpha:]]//g' | grep -qiE "$ENV_BLOCK_PATTERN"
}

pass=0
fail=0

# $1 = expectation (env|real), $2 = case name, stdin = fixture text
check() {
    local expect=$1 name=$2 got
    if classify; then got=env; else got=real; fi
    if [[ "$got" == "$expect" ]]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        printf 'FAIL: %-45s expected=%s got=%s\n' "$name" "$expect" "$got" >&2
    fi
}

# --- ENVIRONMENTAL fixtures: MUST classify as env (return 0) ---------------

check env "bpfjailer-banner" <<<'... blocked on this server based on a security policy ...'
check env "enforcer-fs-reason" <<<'Enforcer: FS, Reason: FILE_OPEN'
check env "cc1-eperm-sysheader" <<<'moduledb.c:1:0: fatal error: /usr/lib/gcc/x86_64-redhat-linux/11/include/stddef.h: Operation not permitted'
check env "cmake-permission-denied" <<<'CMake Error: could not open file X: Permission denied'
check env "cannot-open-eperm" <<<'cannot open output file foo.o: Operation not permitted'
check env "reverie-dbi-custom-build" <<<'error: failed to run custom build command for `reverie-dbi v0.1.0 (/x/reverie-dbi)`'
check env "reverie-dbi-buildrs-panic" <<<"thread 'main' panicked at reverie-dbi/build.rs:339:9"
# Form 4 (this change): binutils EBADF, standalone, WITHOUT the reverie-dbi anchors.
check env "objcopy-ebadf-standalone" <<<'/usr/bin/objcopy: ../bin64/drconfig.debug: Bad file descriptor'
check env "strip-eperm" <<<'strip: build/libfoo.a: Operation not permitted'
check env "ld-permission-denied" <<<'ld: cannot open output file bin/hermit: Permission denied'

# --- REAL failures: MUST NOT classify as env (return non-0) ----------------

check real "guest-ebadf-write" <<<'test fd_hygiene: write(3) failed: Bad file descriptor (os error 9)'
check real "guest-errno-ebadf" <<<'assertion failed: result == Err(Errno(EBADF))'
check real "guest-madvise-eperm" <<<'DETLOG madvise(...) = -1 EPERM (Operation not permitted)'
check real "guest-mount-eperm" <<<'context: Mount, errno: EPERM (Operation not permitted)'
check real "ordinary-test-panic" <<<"thread 'main' panicked at detcore/src/scheduler.rs:42:1: assertion failed"
check real "plain-assertion" <<<'assertion `left == right` failed'
check real "nonzero-exit" <<<'test result: FAILED. 1 passed; 1 failed'

echo "validate-env-block-test: $pass passed, $fail failed"
[[ "$fail" -eq 0 ]]
