#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Twins for scripts/check-linuxemu-unsafe.rs (lead plan U-gate / L1):
#   1 real tree passes against the checked-in baseline
#   2 planted outside-test unsafe refuses
#   3 decreased census refuses until locked in (exact match)
#   4 test-scope unsafe (cfg(test) module and *_tests.rs) still passes
#   5 missing source tree fails closed (rc 2)
#   6 unknown argument fails closed (rc 2)
#   7 raising the baseline without --justify refuses to write (rc 2)
#   8 raising with --justify writes, audits clean, stale justify refuses
#   9 cfg(not(test)) and cfg(any(test, ...)) sites are counted
#  10 writing a missing baseline needs --init; --init on an existing one refuses
#  11 git scope: a hand-raised baseline over HEAD refuses without justify
set -u

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HERMIT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CHECKER="$SCRIPT_DIR/check-linuxemu-unsafe.rs"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

run_checker() {
    last_out="$(rust-script "$CHECKER" "$@" 2>&1)"
    last_rc=$?
}

expect_rc() {
    # expect_rc <wanted> <twin-name>
    if [ "$last_rc" -ne "$1" ]; then
        echo "$last_out" >&2
        fail "twin $2: rc $last_rc, want $1"
    fi
    echo "twin $2: ok (rc $last_rc)"
}

# 1: real tree.
run_checker --repo-root "$HERMIT_ROOT" --skip-negative-control
expect_rc 0 1-real-pass
case "$last_out" in
    *"PASS: LinuxEmu unsafe census matches baseline"*) ;;
    *) echo "$last_out" >&2; fail "twin 1: no PASS line" ;;
esac

SCRATCH=
rc=
for sig in 1 2 3 13 15; do eval "trap 'exit $((sig + 128))' $sig"; done
trap 'rc=$?; set +e; if [ -n "$SCRATCH" ]; then rm -rf "$SCRATCH"; fi; exit $rc' EXIT
SCRATCH="$(mktemp -d)" || exit 1
mkdir -p "$SCRATCH/src/executor"
cat > "$SCRATCH/src/lib.rs" <<'EOF'
fn f() { unsafe {} }
EOF
cat > "$SCRATCH/src/executor/mod.rs" <<'EOF'
unsafe fn g() {}
EOF
cat > "$SCRATCH/baseline.txt" <<'EOF'
total 2
executor/mod.rs 1
lib.rs 1
EOF
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline.txt" --skip-negative-control
expect_rc 0 scratch-pass

# 2: planted outside-test unsafe refuses.
cat >> "$SCRATCH/src/lib.rs" <<'EOF'
fn h() { unsafe {} }
EOF
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline.txt" --skip-negative-control
expect_rc 1 2-planted-refuses

# 3: a decrease refuses until --write-baseline locks it in.
cat > "$SCRATCH/baseline3.txt" <<'EOF'
total 3
executor/mod.rs 1
lib.rs 2
EOF
cat > "$SCRATCH/src/lib.rs" <<'EOF'
fn f() { unsafe {} }
EOF
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline3.txt" --skip-negative-control
expect_rc 1 3-decrease-refuses-unlocked
case "$last_out" in
    *"run --write-baseline to lock in the decrease"*) ;;
    *) echo "$last_out" >&2; fail "twin 3: no lock-in message" ;;
esac
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline3.txt" --write-baseline --skip-negative-control
expect_rc 0 3-decrease-locks-in
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline3.txt" --skip-negative-control
expect_rc 0 3-locked-in-passes

# 4: test-scope unsafe does not move the census.
cat >> "$SCRATCH/src/lib.rs" <<'EOF'
#[cfg(test)]
mod tests {
    fn t() { unsafe {} }
}
EOF
cat > "$SCRATCH/src/extra_tests.rs" <<'EOF'
fn t() { unsafe {} }
EOF
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline.txt" --skip-negative-control
expect_rc 0 4-test-scope-pass

# 5: missing source tree fails closed.
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/nope" \
    --baseline "$SCRATCH/baseline.txt" --skip-negative-control
expect_rc 2 5-missing-src-rc2

# 6: unknown argument fails closed.
run_checker --bogus
expect_rc 2 6-unknown-arg-rc2

# 7+8: the write gate. lib.rs currently holds 1 site (plus the cfg(test)
# module from twin 4, which does not count); plant a second site, then
# raising the baseline needs a justification.
cat >> "$SCRATCH/src/lib.rs" <<'EOF'
fn h2() { unsafe {} }
EOF
cp "$SCRATCH/baseline.txt" "$SCRATCH/baseline-raise.txt"
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline-raise.txt" --write-baseline --skip-negative-control
expect_rc 2 7-raise-without-justify-refuses
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline-raise.txt" --write-baseline \
    --justify "lib.rs=twin 8 planted site, justify gate proof" --skip-negative-control
expect_rc 0 8-raise-with-justify-writes
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline-raise.txt" --skip-negative-control
expect_rc 0 8-justified-audit-passes
sed -i 's/^justify lib\.rs 2 /justify lib.rs 9 /' "$SCRATCH/baseline-raise.txt"
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline-raise.txt" --skip-negative-control
expect_rc 2 8-stale-justify-refuses

# 9: production-reachable cfg shapes never hide a site.
cat > "$SCRATCH/src/lib.rs" <<'EOF'
fn f() { unsafe {} }
#[cfg(not(test))]
fn g() { unsafe {} }
#[cfg(any(test, feature = "test-support"))]
fn h() { unsafe {} }
#[cfg(all(test, feature = "x"))]
fn t() { unsafe {} }
EOF
cat > "$SCRATCH/baseline9.txt" <<'EOF'
total 2
executor/mod.rs 1
lib.rs 1
EOF
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline9.txt" --skip-negative-control
expect_rc 1 9-cfg-shapes-counted
cat > "$SCRATCH/baseline9.txt" <<'EOF'
total 4
executor/mod.rs 1
lib.rs 3
EOF
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline9.txt" --skip-negative-control
expect_rc 0 9-cfg-shapes-exact

# 10: creating a baseline needs --init; --init over an existing one refuses.
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline-new.txt" --write-baseline --skip-negative-control
expect_rc 2 10-init-required
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline-new.txt" --write-baseline --init --skip-negative-control
expect_rc 0 10-init-writes
run_checker --repo-root "$SCRATCH" --linuxemu-src "$SCRATCH/src" \
    --baseline "$SCRATCH/baseline-new.txt" --write-baseline --init --skip-negative-control
expect_rc 2 10-init-existing-refuses

# 11: git scope base comparison. A committed baseline, then a planted
# site with the baseline hand-raised to match: audit must still refuse
# (no justification over HEAD); a justify line for the raised count
# passes. Git is required: its absence fails this twin, never skips it.
command -v git >/dev/null 2>&1 || fail "twin 11: git is required"
if true; then
    mkdir -p "$SCRATCH/repo/src"
    cat > "$SCRATCH/repo/src/lib.rs" <<'EOF'
fn f() { unsafe {} }
EOF
    cat > "$SCRATCH/repo/baseline.txt" <<'EOF'
total 1
lib.rs 1
EOF
    git -C "$SCRATCH/repo" init -q || fail "twin 11: git init failed"
    git -C "$SCRATCH/repo" config user.email twin@example.invalid
    git -C "$SCRATCH/repo" config user.name twin
    git -C "$SCRATCH/repo" add src/lib.rs baseline.txt || fail "twin 11: git add failed"
    git -C "$SCRATCH/repo" commit -qm base || fail "twin 11: git commit failed"
    cat >> "$SCRATCH/repo/src/lib.rs" <<'EOF'
fn g() { unsafe {} }
EOF
    cat > "$SCRATCH/repo/baseline.txt" <<'EOF'
total 2
lib.rs 2
EOF
    run_checker --repo-root "$SCRATCH/repo" --linuxemu-src "$SCRATCH/repo/src" \
        --baseline "$SCRATCH/repo/baseline.txt" --skip-negative-control
    expect_rc 1 11-hand-raise-refuses
    case "$last_out" in
        *"BASE: HEAD baseline compared"*) ;;
        *) echo "$last_out" >&2; fail "twin 11: no BASE compared line" ;;
    esac
    cat >> "$SCRATCH/repo/baseline.txt" <<'EOF'
justify lib.rs 2 twin 11 planted site, base-compare proof
EOF
    run_checker --repo-root "$SCRATCH/repo" --linuxemu-src "$SCRATCH/repo/src" \
        --baseline "$SCRATCH/repo/baseline.txt" --skip-negative-control
    expect_rc 0 11-justified-over-head-passes
fi

echo "PASS: check-linuxemu-unsafe twins (11/11)"
