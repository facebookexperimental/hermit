#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Functional compatibility workloads for validate.sh's strict L2 matrix.

set -euo pipefail

if (($# != 1)); then
    echo "usage: $0 PROGRAM" >&2
    exit 2
fi

readonly PROGRAM=$1
readonly FIXTURE_ROOT=${REAL_COMPAT_FIXTURES:-/tmp/hermit-real-compat-fixtures}
readonly WORK_DIR="/tmp/hermit-real-compat-$PROGRAM"
export LC_ALL=C
export TZ=UTC
umask 022

rm -rf "$WORK_DIR"
mkdir -p "$WORK_DIR"
trap 'rm -rf "$WORK_DIR"' EXIT

function write_assembly_fixture {
    cat >"$WORK_DIR/add.s" <<'EOF'
    .text
    .globl compat_add
    .type compat_add,@function
compat_add:
    lea (%rdi,%rsi), %eax
    ret
    .size compat_add, .-compat_add
    .section .note.GNU-stack,"",@progbits
EOF
}

function build_assembly_object {
    write_assembly_fixture
    /usr/bin/as --64 "$WORK_DIR/add.s" -o "$WORK_DIR/add.o"
}

case "$PROGRAM" in
    cargo)
        mkdir -p "$WORK_DIR/src"
        cat >"$WORK_DIR/Cargo.toml" <<'EOF'
[package]
name = "hermit-real-compat"
version = "0.1.0"
edition = "2021"
description = "Hermit functional compatibility fixture"
license = "BSD-3-Clause"

[workspace]
EOF
        cat >"$WORK_DIR/src/lib.rs" <<'EOF'
pub fn weighted_sum(values: &[u64]) -> u64 {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| (index as u64 + 1) * value)
        .sum()
}
EOF
        cargo metadata --offline --format-version 1 --no-deps \
            --manifest-path "$WORK_DIR/Cargo.toml" >"$WORK_DIR/metadata.json"
        cargo package --offline --allow-dirty --list \
            --manifest-path "$WORK_DIR/Cargo.toml" >"$WORK_DIR/package-files.txt"
        grep -q '"name":"hermit-real-compat"' "$WORK_DIR/metadata.json"
        grep -qx 'src/lib.rs' "$WORK_DIR/package-files.txt"
        printf 'cargo:metadata-and-package-list\n'
        ;;
    rustc)
        cat >"$WORK_DIR/main.rs" <<'EOF'
fn main() {
    let sum: u64 = (1..=100).map(|value| value * value).sum();
    assert_eq!(sum, 338350);
    println!("rustc:{sum}");
}
EOF
        # GCC's linker driver races vfork/pipe completion under L2.
        # Clang keeps the ordering stable; suppress its build ID as well.
        rustc --crate-name hermit_real_compat -C opt-level=1 -C debuginfo=0 \
            -C metadata=hermit-real-compat -C linker=/usr/bin/clang \
            -C link-arg=-Wl,--build-id=none \
            "$WORK_DIR/main.rs" -o "$WORK_DIR/program"
        "$WORK_DIR/program"
        ;;
    clang)
        cat >"$WORK_DIR/main.c" <<'EOF'
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>

int main(void) {
    uint64_t factorial = 1;
    for (uint64_t value = 2; value <= 20; ++value) {
        factorial *= value;
    }
    if (factorial != UINT64_C(2432902008176640000)) {
        return 1;
    }
    printf("clang:%" PRIu64 "\n", factorial);
    return 0;
}
EOF
        /usr/bin/clang -O2 -Wl,--build-id=none \
            "$WORK_DIR/main.c" -o "$WORK_DIR/program"
        "$WORK_DIR/program"
        ;;
    javac)
        cat >"$WORK_DIR/CompilerCompat.java" <<'EOF'
public final class CompilerCompat {
    public static void main(String[] args) {
        long previous = 0;
        long current = 1;
        for (int index = 0; index < 30; ++index) {
            long next = previous + current;
            previous = current;
            current = next;
        }
        if (previous != 832040) {
            throw new AssertionError(previous);
        }
        System.out.println("javac:" + previous);
    }
}
EOF
        # Avoid live NSS queries while the JVM initializes user properties.
        javac -J-Duser.name=hermit -J-Duser.home="$WORK_DIR" \
            -J-Xint -J-XX:+UseSerialGC -J-XX:ActiveProcessorCount=1 \
            -g:none -d "$WORK_DIR" "$WORK_DIR/CompilerCompat.java"
        # Direct execution avoids a parent/child command-substitution pipe.
        java -Duser.name=hermit -Duser.home="$WORK_DIR" \
            -Xint -XX:+UseSerialGC -XX:ActiveProcessorCount=1 \
            -cp "$WORK_DIR" CompilerCompat
        ;;
    java)
        cat >"$WORK_DIR/Compat.java" <<'EOF'
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.FutureTask;

class Compat {
    public static void main(String[] args) throws Exception {
        Path path = Paths.get("/tmp/hermit-real-compat-java/data.txt");
        Files.write(path, Arrays.asList("gamma", "alpha", "beta"), StandardCharsets.UTF_8);
        List<String> lines = new ArrayList<>(Files.readAllLines(path));
        Collections.sort(lines);
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        byte[] hash = digest.digest(String.join(":", lines).getBytes(StandardCharsets.UTF_8));
        FutureTask<Integer> task = new FutureTask<>(() -> lines.stream().mapToInt(String::length).sum());
        Thread worker = new Thread(task, "compat-worker");
        worker.start();
        int total = task.get();
        StringBuilder prefix = new StringBuilder();
        for (int index = 0; index < 4; index++) {
            prefix.append(String.format("%02x", hash[index]));
        }
        System.out.printf("java:%d:%s:%b%n", total, prefix, System.currentTimeMillis() > 0);
    }
}
EOF
        # Bound JVM-internal workers while retaining Compat's application thread.
        javac -J-Xint -J-XX:+UseSerialGC -J-XX:ActiveProcessorCount=1 \
            -d "$WORK_DIR" "$WORK_DIR/Compat.java"
        java -Xint -XX:+UseSerialGC -XX:ActiveProcessorCount=1 \
            -cp "$WORK_DIR" Compat
        ;;
    git)
        readonly GIT=/usr/local/bin/git.meta.real
        mkdir -p "$WORK_DIR/home" "$WORK_DIR/repo"
        export HOME="$WORK_DIR/home"
        export GIT_CONFIG_NOSYSTEM=1
        "$GIT" -C "$WORK_DIR/repo" init -q
        "$GIT" -C "$WORK_DIR/repo" config user.name "Hermit Compat"
        "$GIT" -C "$WORK_DIR/repo" config user.email hermit@example.invalid
        "$GIT" -C "$WORK_DIR/repo" config commit.gpgsign false
        printf 'alpha\nbeta\n' >"$WORK_DIR/repo/data.txt"
        "$GIT" -C "$WORK_DIR/repo" add data.txt
        GIT_AUTHOR_DATE='2000-01-01T00:00:00Z' \
        GIT_COMMITTER_DATE='2000-01-01T00:00:00Z' \
            "$GIT" -C "$WORK_DIR/repo" commit -q -m 'compat commit'
        printf 'gamma\n' >>"$WORK_DIR/repo/data.txt"
        "$GIT" -C "$WORK_DIR/repo" diff --no-ext-diff -- data.txt >"$WORK_DIR/diff"
        grep -Fq '+gamma' "$WORK_DIR/diff"
        subject=$("$GIT" -C "$WORK_DIR/repo" log -1 --format=%s)
        blob=$("$GIT" -C "$WORK_DIR/repo" rev-parse HEAD:data.txt)
        printf 'git:%s:%s\n' "$subject" "$blob"
        ;;
    gcc)
        cat >"$WORK_DIR/fixture.c" <<'EOF'
#include <stddef.h>

int hermit_weighted(const int *values, size_t length) {
    int total = 0;
    for (size_t index = 0; index < length; ++index) {
        total += (int)(index + 1) * values[index];
    }
    return total;
}
EOF
        gcc -std=c11 -O2 -Wall -Wextra -fno-ident -frandom-seed=hermit-gcc \
            -c "$WORK_DIR/fixture.c" -o "$WORK_DIR/fixture.o"
        nm -g --defined-only "$WORK_DIR/fixture.o" | grep -q ' T hermit_weighted$'
        printf 'gcc:object-with-hermit_weighted\n'
        ;;
    g++)
        cat >"$WORK_DIR/fixture.cpp" <<'EOF'
#include <algorithm>
#include <numeric>
#include <vector>

extern "C" long hermit_sorted_sum(const int* input, std::size_t length) {
    std::vector<int> values(input, input + length);
    std::sort(values.begin(), values.end());
    return std::accumulate(values.begin(), values.end(), 0L);
}
EOF
        g++ -std=c++17 -O2 -Wall -Wextra -fno-ident -frandom-seed=hermit-gxx \
            -S "$WORK_DIR/fixture.cpp" -o "$WORK_DIR/fixture.s"
        grep -q '^hermit_sorted_sum:' "$WORK_DIR/fixture.s"
        printf 'g++:assembly-with-hermit_sorted_sum\n'
        ;;
    make)
        printf '6\n7\n' >"$WORK_DIR/input.txt"
        {
            printf '%s\n' 'all: result.txt' 'result.txt: input.txt'
            printf '\t@printf "make:42\\n" > result.txt\n'
        } >"$WORK_DIR/Makefile"
        make --no-print-directory -s -C "$WORK_DIR"
        make --no-print-directory -q -C "$WORK_DIR"
        IFS= read -r result <"$WORK_DIR/result.txt"
        test "$result" = 'make:42'
        printf '%s\n' "$result"
        ;;
    ar)
        archive=$(gcc -print-file-name=libgcc.a)
        test -r "$archive"
        /usr/bin/ar t "$archive" | grep -qx '_muldi3.o'
        bytes=$(/usr/bin/ar p "$archive" _muldi3.o | wc -c)
        test "$bytes" -gt 0
        printf 'ar:_muldi3.o:%s-bytes\n' "$bytes"
        ;;
    as)
        build_assembly_object
        /usr/bin/readelf -sW "$WORK_DIR/add.o" | grep -Eq 'FUNC.*GLOBAL.*compat_add'
        printf 'as:compat_add\n'
        ;;
    ld)
        cat >"$WORK_DIR/start.s" <<'EOF'
    .text
    .globl _start
_start:
    mov $60, %rax
    xor %rdi, %rdi
    syscall
EOF
        /usr/bin/as --64 "$WORK_DIR/start.s" -o "$WORK_DIR/start.o"
        /usr/bin/ld --build-id=none -o "$WORK_DIR/program" "$WORK_DIR/start.o"
        "$WORK_DIR/program"
        printf 'ld:exit-0\n'
        ;;
    nm)
        build_assembly_object
        /usr/bin/nm --defined-only "$WORK_DIR/add.o" | grep -E ' [Tt] compat_add$'
        ;;
    objcopy)
        build_assembly_object
        printf 'compat-section\n' >"$WORK_DIR/section.txt"
        /usr/bin/objcopy --add-section .compat="$WORK_DIR/section.txt" \
            --set-section-flags .compat=readonly,data \
            "$WORK_DIR/add.o" "$WORK_DIR/with-section.o"
        /usr/bin/readelf -SW "$WORK_DIR/with-section.o" | grep -q '\.compat'
        printf 'objcopy:.compat\n'
        ;;
    objdump)
        build_assembly_object
        /usr/bin/objdump -d "$WORK_DIR/add.o" >"$WORK_DIR/disassembly"
        grep -q '<compat_add>:' "$WORK_DIR/disassembly"
        grep -Eq '[[:space:]]ret[q]?[[:space:]]*$' "$WORK_DIR/disassembly"
        printf 'objdump:compat_add:ret\n'
        ;;
    ranlib)
        archive=$(gcc -print-file-name=libgcc.a)
        test -r "$archive"
        cp "$archive" "$WORK_DIR/libcompat.a"
        /usr/bin/ranlib -D "$WORK_DIR/libcompat.a"
        /usr/bin/nm -s "$WORK_DIR/libcompat.a" >"$WORK_DIR/archive-index.txt"
        grep -q ' in _muldi3.o$' "$WORK_DIR/archive-index.txt"
        printf 'ranlib:indexed-libgcc-copy\n'
        ;;
    readelf)
        build_assembly_object
        /usr/bin/readelf -hSWs "$WORK_DIR/add.o" >"$WORK_DIR/readelf.out"
        grep -q 'ELF64' "$WORK_DIR/readelf.out"
        grep -q '\.text' "$WORK_DIR/readelf.out"
        grep -q 'compat_add' "$WORK_DIR/readelf.out"
        printf 'readelf:ELF64:.text:compat_add\n'
        ;;
    size)
        test -s "$FIXTURE_ROOT/binutils/with-symbols.o"
        /usr/bin/size -A "$FIXTURE_ROOT/binutils/with-symbols.o" >"$WORK_DIR/size.out"
        awk '$1 == ".text" && $2 > 0 { found = 1 } END { exit !found }' "$WORK_DIR/size.out"
        awk '$1 == ".text" { print "size:.text:" $2 }' "$WORK_DIR/size.out"
        ;;
    strip)
        test -s "$FIXTURE_ROOT/binutils/with-symbols.o"
        cp -p "$FIXTURE_ROOT/binutils/with-symbols.o" "$WORK_DIR/with-symbols.o"
        /usr/bin/strip --strip-all "$WORK_DIR/with-symbols.o" \
            -o "$WORK_DIR/stripped.o"
        ! /usr/bin/readelf -SW "$WORK_DIR/stripped.o" | grep -q '\.symtab'
        printf 'strip:relocatable-object:no-symtab\n'
        ;;
    addr2line)
        test -s "$FIXTURE_ROOT/binutils/with-symbols.o"
        address=$(/usr/bin/nm "$FIXTURE_ROOT/binutils/with-symbols.o" | \
            awk '$3 == "compat_line" { print $1 }')
        /usr/bin/addr2line -f -e "$FIXTURE_ROOT/binutils/with-symbols.o" \
            "$address" >"$WORK_DIR/location"
        grep -Fxq 'compat_line' "$WORK_DIR/location"
        tail -n 1 "$WORK_DIR/location" | grep -Eq 'fixture.c:[0-9]+$'
        printf 'addr2line:%s\n' "$(tail -n 1 "$WORK_DIR/location" | sed 's|.*/||')"
        ;;
    c++filt)
        demangled=$(printf '_ZN6hermit6compatEi\n' | /usr/bin/c++filt)
        test "$demangled" = 'hermit::compat(int)'
        printf 'c++filt:%s\n' "$demangled"
        ;;
    elfedit)
        build_assembly_object
        cp "$WORK_DIR/add.o" "$WORK_DIR/edited.o"
        /usr/bin/elfedit --output-osabi GNU "$WORK_DIR/edited.o"
        /usr/bin/readelf -h "$WORK_DIR/edited.o" | grep -q 'UNIX - GNU'
        printf 'elfedit:GNU\n'
        ;;
    gprof)
        test -x "$FIXTURE_ROOT/gprof/program"
        test -s "$FIXTURE_ROOT/gprof/gmon.out"
        /usr/bin/gprof -b "$FIXTURE_ROOT/gprof/program" \
            "$FIXTURE_ROOT/gprof/gmon.out" >"$WORK_DIR/profile.out"
        grep -q 'compat_root' "$WORK_DIR/profile.out"
        grep -q 'compat_leaf' "$WORK_DIR/profile.out"
        printf 'gprof:root:leaf\n'
        ;;
    cpp)
        cat >"$WORK_DIR/compat.h" <<'EOF'
#define PRODUCT(left, right) ((left) * (right))
EOF
        cat >"$WORK_DIR/input.c" <<'EOF'
#include "compat.h"
int value = PRODUCT(6, 7);
EOF
        /usr/bin/cpp -P -I"$WORK_DIR" "$WORK_DIR/input.c" >"$WORK_DIR/output.c"
        grep -Eq '^int value = .*6.*7.*;$' "$WORK_DIR/output.c"
        printf 'cpp:'
        tr -d ' ' <"$WORK_DIR/output.c"
        ;;
    gcov)
        test -s "$FIXTURE_ROOT/gcov/coverage.gcno"
        test -s "$FIXTURE_ROOT/gcov/coverage.gcda"
        cp -p "$FIXTURE_ROOT/gcov/coverage.c" "$WORK_DIR/coverage.c"
        (cd "$WORK_DIR" && /usr/bin/gcov -b -c \
            -o "$FIXTURE_ROOT/gcov" coverage.c >gcov.out)
        grep -q 'compat_marker' "$WORK_DIR/coverage.c.gcov"
        grep -Eq '^[[:space:]]*[1-9][0-9]*:.*compat_marker' "$WORK_DIR/coverage.c.gcov"
        printf 'gcov:covered-marker\n'
        ;;
    *)
        echo "unknown real compatibility workload: $PROGRAM" >&2
        exit 2
        ;;
esac
