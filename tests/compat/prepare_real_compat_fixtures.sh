#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Build immutable inputs consumed by functional strict L2 probes.

set -euo pipefail

if (($# != 1)); then
    echo "usage: $0 FIXTURE_ROOT" >&2
    exit 2
fi

readonly FINAL_ROOT=$1
# Build in a staging sibling and publish by rename. Two prep nodes (host and
# pinned-root twin) can run in one validation; a reader must never observe
# the rm -rf/rebuild window or a half-written tree (issue 4997). The staging
# name is fixed (no PID), so two builds of the same final path are byte
# identical; a lock file serializes concurrent builders of that path.
readonly BUILD_ROOT="${FINAL_ROOT}.build"
readonly OLD_ROOT="${FINAL_ROOT}.old"
# The lock lives beside the fixture root; its parent (the run state's
# strict-compat directory) may not exist yet on a fresh run state.
mkdir -p "${FINAL_ROOT%/*}"
exec 9>"${FINAL_ROOT}.prep.lock"
flock 9
trap 'rm -rf "$BUILD_ROOT" "$OLD_ROOT"' EXIT
rm -rf "$BUILD_ROOT" "$OLD_ROOT"
FIXTURE_ROOT=$BUILD_ROOT
mkdir -p "$FIXTURE_ROOT/binutils" "$FIXTURE_ROOT/gprof" "$FIXTURE_ROOT/gcov" \
    "$FIXTURE_ROOT/lsof" "$FIXTURE_ROOT/df" "$FIXTURE_ROOT/toolchain" \
    "$FIXTURE_ROOT/java"

# Compatibility probes run from an otherwise-empty /test. Snapshot the one
# repository input used by the coreutils rows instead of accidentally relying
# on the validate driver's current directory.
cp "$PWD/README.md" "$FIXTURE_ROOT/README.md"

# `--base-env=minimal` deliberately excludes the user's rustup directory. The
# cargo workload still needs the active toolchain, so expose exactly cargo and
# rustc through this run-owned fixture rather than passing through the host PATH.
# On a host the active toolchain is rustup's; in the pinned root there is no
# rustup and the toolchain is the image's own, on PATH. Resolve in whichever
# environment prepares the fixtures, so the links always name binaries of the
# environment the corpus will run in.
if command -v rustup >/dev/null 2>&1; then
    ln -s "$(rustup which cargo)" "$FIXTURE_ROOT/toolchain/cargo"
    ln -s "$(rustup which rustc)" "$FIXTURE_ROOT/toolchain/rustc"
else
    ln -s "$(command -v cargo)" "$FIXTURE_ROOT/toolchain/cargo"
    ln -s "$(command -v rustc)" "$FIXTURE_ROOT/toolchain/rustc"
fi

gcc -O2 -Wall -Wextra -Werror -Wl,--build-id=none \
    "$PWD/tests/compat/localhost_http_server.c" \
    -o "$FIXTURE_ROOT/localhost-http-server"

cat >"$FIXTURE_ROOT/binutils/fixture.c" <<'EOF'
__attribute__((noinline)) int compat_line(int value) {
    return value + 1;
}
EOF
gcc -g -O0 -fno-ident -frandom-seed=hermit-binutils \
    -ffile-prefix-map="$FIXTURE_ROOT=." -fdebug-prefix-map="$FIXTURE_ROOT=." \
    -c "$FIXTURE_ROOT/binutils/fixture.c" \
    -o "$FIXTURE_ROOT/binutils/with-symbols.o"
/usr/bin/readelf -SW "$FIXTURE_ROOT/binutils/with-symbols.o" | grep -q '\.symtab'

cat >"$FIXTURE_ROOT/gprof/profile.c" <<'EOF'
#include <stdio.h>
volatile unsigned long sink;
__attribute__((noinline)) void compat_leaf(unsigned long value) { sink += value; }
__attribute__((noinline)) void compat_root(void) {
    for (unsigned long index = 0; index < 1000000; ++index) compat_leaf(index & 7);
}
int main(void) { compat_root(); printf("%lu\n", sink); return 0; }
EOF
gcc -O0 -pg -fno-pie -no-pie -Wl,--build-id=none \
    "$FIXTURE_ROOT/gprof/profile.c" -o "$FIXTURE_ROOT/gprof/program"
(
    cd "$FIXTURE_ROOT/gprof"
    ./program >program.out
)
test -s "$FIXTURE_ROOT/gprof/gmon.out"

cat >"$FIXTURE_ROOT/gcov/coverage.c" <<'EOF'
#include <stdio.h>
int main(void) {
    int total = 0; /* compat_marker */
    for (int index = 0; index < 5; ++index) {
        if (index % 2 == 0) total += index;
    }
    printf("%d\n", total);
    return total != 6;
}
EOF
(
    cd "$FIXTURE_ROOT/gcov"
    gcc --coverage -O0 -fno-ident -frandom-seed=hermit-gcov \
        -ffile-prefix-map="$FIXTURE_ROOT=." -fdebug-prefix-map="$FIXTURE_ROOT=." \
        -fprofile-prefix-path="$FIXTURE_ROOT" \
        -Wl,--build-id=none coverage.c -o coverage
    ./coverage >program.out
)
test -s "$FIXTURE_ROOT/gcov/coverage.gcno"
test -s "$FIXTURE_ROOT/gcov/coverage.gcda"

# lsof unconditionally walks /proc/mounts before applying its PID/FD filters.
# Serve that read from an inherited descriptor for a fixed, valid mount table
# so other users creating or removing host mounts cannot change the
# strict-verify syscall stream.  The preload library refuses a descriptor on
# procfs, so pointing it back at the live table cannot satisfy the marker.
gcc -shared -fPIC -Wall -Wextra -Werror \
    "$PWD/tests/compat/lsof_mount_redirect.c" -ldl \
    -o "$FIXTURE_ROOT/lsof/libmount_redirect.so"
cat >"$FIXTURE_ROOT/lsof/mounts" <<'EOF'
fixture /tmp fixture rw 0 0
EOF

# `df` consults mountinfo before statfs(2). Hermit's private mount roots differ
# between the two executions inside --verify, so give this command-compatibility
# probe a fixed, valid view of the root mount. The product-level mountinfo
# determinization remains covered separately; this fixture does not weaken or
# filter the verifier.
cat >"$FIXTURE_ROOT/df/mountinfo" <<'EOF'
1 0 0:1 / / rw,relatime - rootfs rootfs rw
EOF

# The split java/javac rows (2026-11-05): the run cells execute classes
# compiled here from the tracked sources the compile cells also compile, so
# each guest pays one JVM startup instead of two. Same javac flags as the
# workload rows (CompilerCompat with -g:none, Compat with defaults), so the
# class bytes are the bytes the compile cells produce and print.
cp "$PWD/tests/compat/Compat.java" "$PWD/tests/compat/CompilerCompat.java" \
    "$FIXTURE_ROOT/java/"
javac -cp "$FIXTURE_ROOT/java" -d "$FIXTURE_ROOT/java" \
    "$FIXTURE_ROOT/java/Compat.java"
javac -g:none -cp "$FIXTURE_ROOT/java" -d "$FIXTURE_ROOT/java" \
    "$FIXTURE_ROOT/java/CompilerCompat.java"
test -s "$FIXTURE_ROOT/java/Compat.class"
test -s "$FIXTURE_ROOT/java/CompilerCompat.class"

# Prevent host build time from becoming guest-visible input.
find "$FIXTURE_ROOT" -exec touch -h -d @1 {} +
# Content manifest, so a test can prove two preparations are the same inputs.
(
    cd "$FIXTURE_ROOT"
    find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum >SHA256SUMS
    find . -type l -print0 | sort -z | while IFS= read -r -d '' link; do
        printf 'SYMLINK %s -> %s\n' "$link" "$(readlink "$link")"
    done >>SHA256SUMS
)
# Atomic publish: rename the old tree aside, rename the new tree in, then drop
# the old tree. A concurrent reader sees the whole old tree or the whole new
# tree, never a missing or partial one.
if [[ -e "$FINAL_ROOT" ]]; then
    mv "$FINAL_ROOT" "$OLD_ROOT"
fi
mv "$BUILD_ROOT" "$FINAL_ROOT"
rm -rf "$OLD_ROOT"
trap - EXIT
printf 'prepared real compatibility fixtures: %s\n' "$FINAL_ROOT"
