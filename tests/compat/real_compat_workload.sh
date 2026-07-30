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

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#686): Review archive reproducibility and localhost fixture cleanup.
function prepare_archive_fixture {
    mkdir -p "$WORK_DIR/source" "$WORK_DIR/output"
    printf 'Hermit archive payload\nline two\n' >"$WORK_DIR/source/payload.txt"
    touch -t 200001010000 "$WORK_DIR/source/payload.txt"
}

function verify_archive_roundtrip {
    local archive=$1
    local archive_digest
    local payload_digest

    cmp "$WORK_DIR/source/payload.txt" "$WORK_DIR/output/payload.txt"
    archive_digest=$(sha256sum "$archive" | cut -d' ' -f1)
    payload_digest=$(sha256sum "$WORK_DIR/output/payload.txt" | cut -d' ' -f1)
    printf '%s:%s:%s\n' "$PROGRAM" "$archive_digest" "$payload_digest"
}

function fetch_localhost_payload {
    (
        local client=$1
        local response_bytes
        local server_pid=
        local server_status=0
        local status=0

        trap 'if [[ -n $server_pid ]]; then kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi' EXIT
        prepare_archive_fixture
        response_bytes=$(wc -c <"$WORK_DIR/source/payload.txt")
        {
            printf 'HTTP/1.0 200 OK\r\nContent-Length: %s\r\nConnection: close\r\n\r\n' \
                "$response_bytes"
            cat "$WORK_DIR/source/payload.txt"
        } >"$WORK_DIR/response.http"

        /usr/bin/nc --send-only -l 127.0.0.1 18765 \
            <"$WORK_DIR/response.http" >"$WORK_DIR/server.log" 2>&1 &
        server_pid=$!

        # Yield one deterministic logical interval so Ncat reaches listen(2)
        # before the client connects, without a second readiness process.
        sleep 0.1
        if [[ $client == wget ]]; then
            /usr/bin/wget --quiet \
                --output-document="$WORK_DIR/output/payload.txt" \
                http://127.0.0.1:18765/payload.txt || status=$?
        else
            /usr/bin/curl --fail --silent --show-error \
                --output "$WORK_DIR/output/payload.txt" \
                http://127.0.0.1:18765/payload.txt || status=$?
        fi

        if ((status != 0)); then
            kill "$server_pid" 2>/dev/null || true
            wait "$server_pid" 2>/dev/null || true
        elif wait "$server_pid"; then
            :
        else
            server_status=$?
        fi
        server_pid=
        if ((status != 0)); then
            return "$status"
        fi
        if ((server_status != 0)); then
            return "$server_status"
        fi

        cmp "$WORK_DIR/source/payload.txt" "$WORK_DIR/output/payload.txt"
        printf '%s:' "$PROGRAM"
        sha256sum "$WORK_DIR/output/payload.txt" | cut -d' ' -f1
    )
}

case "$PROGRAM" in
    gzip-roundtrip)
        prepare_archive_fixture
        gzip -n -c "$WORK_DIR/source/payload.txt" >"$WORK_DIR/archive.gz"
        gzip -dc "$WORK_DIR/archive.gz" >"$WORK_DIR/output/payload.txt"
        verify_archive_roundtrip "$WORK_DIR/archive.gz"
        ;;
    bzip2-roundtrip)
        prepare_archive_fixture
        bzip2 -c "$WORK_DIR/source/payload.txt" >"$WORK_DIR/archive.bz2"
        bzip2 -dc "$WORK_DIR/archive.bz2" >"$WORK_DIR/output/payload.txt"
        verify_archive_roundtrip "$WORK_DIR/archive.bz2"
        ;;
    xz-roundtrip)
        prepare_archive_fixture
        xz -c "$WORK_DIR/source/payload.txt" >"$WORK_DIR/archive.xz"
        xz -dc "$WORK_DIR/archive.xz" >"$WORK_DIR/output/payload.txt"
        verify_archive_roundtrip "$WORK_DIR/archive.xz"
        ;;
    zstd-roundtrip)
        prepare_archive_fixture
        zstd -q -c "$WORK_DIR/source/payload.txt" >"$WORK_DIR/archive.zst"
        zstd -q -d -c "$WORK_DIR/archive.zst" >"$WORK_DIR/output/payload.txt"
        verify_archive_roundtrip "$WORK_DIR/archive.zst"
        ;;
    tar-roundtrip)
        prepare_archive_fixture
        tar --format=ustar --sort=name --mtime=@946684800 \
            --owner=0 --group=0 --numeric-owner \
            -cf "$WORK_DIR/archive.tar" -C "$WORK_DIR/source" payload.txt
        tar -xf "$WORK_DIR/archive.tar" -C "$WORK_DIR/output"
        verify_archive_roundtrip "$WORK_DIR/archive.tar"
        ;;
    cpio-roundtrip)
        prepare_archive_fixture
        (
            cd "$WORK_DIR/source"
            printf 'payload.txt\n' | cpio --quiet --reproducible \
                --renumber-inodes -o -H newc
        ) >"$WORK_DIR/archive.cpio"
        (cd "$WORK_DIR/output" && cpio --quiet -id <"$WORK_DIR/archive.cpio")
        verify_archive_roundtrip "$WORK_DIR/archive.cpio"
        ;;
    wget-localhost)
        fetch_localhost_payload wget
        ;;
    curl-localhost)
        fetch_localhost_payload curl
        ;;
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
        if [[ -x /usr/local/bin/git.meta.real ]]; then
            readonly GIT=/usr/local/bin/git.meta.real
        else
            readonly GIT=/usr/bin/git
        fi
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
    sqlite3)
        output=$(
            /usr/bin/sqlite3 -batch -noheader -separator : :memory: <<'EOF'
CREATE TABLE measurements(category TEXT NOT NULL, value INTEGER NOT NULL);
INSERT INTO measurements VALUES
    ('alpha', 6), ('beta', 7), ('alpha', 15), ('beta', 14);
SELECT category, count(*), sum(value)
FROM measurements
GROUP BY category
ORDER BY category;
EOF
        )
        test "$output" = $'alpha:2:21\nbeta:2:21'
        printf 'sqlite3:groups-ok\n'
        ;;
    jq)
        cat >"$WORK_DIR/input.json" <<'EOF'
[
  {"name": "gamma", "value": 21},
  {"name": "beta", "value": 6},
  {"name": "alpha", "value": 15}
]
EOF
        output=$(/usr/bin/jq -c \
            '{total: (map(.value) | add), selected: ([.[] | select(.value >= 10) | .name] | sort)}' \
            "$WORK_DIR/input.json")
        test "$output" = '{"total":42,"selected":["alpha","gamma"]}'
        printf 'jq:aggregate-ok\n'
        ;;
    xmllint)
        cat >"$WORK_DIR/inventory.xml" <<'EOF'
<?xml version="1.0"?>
<!DOCTYPE inventory [
<!ELEMENT inventory (item+)>
<!ELEMENT item EMPTY>
<!ATTLIST item name CDATA #REQUIRED value CDATA #REQUIRED>
]>
<inventory>
  <item name="alpha" value="6"/>
  <item name="beta" value="15"/>
  <item name="gamma" value="21"/>
</inventory>
EOF
        /usr/bin/xmllint --nonet --valid --noout "$WORK_DIR/inventory.xml"
        count=$(/usr/bin/xmllint --xpath 'count(/inventory/item)' \
            "$WORK_DIR/inventory.xml")
        sum=$(/usr/bin/xmllint --xpath 'sum(/inventory/item/@value)' \
            "$WORK_DIR/inventory.xml")
        test "$count:$sum" = '3:42'
        printf 'xmllint:valid:count=%s:sum=%s\n' "$count" "$sum"
        ;;
    cmake)
        cat >"$WORK_DIR/workload.cmake" <<'EOF'
math(EXPR product "6 * 7")
if(NOT product EQUAL 42)
  message(FATAL_ERROR "unexpected product: ${product}")
endif()
file(WRITE "${OUTPUT_PATH}" "cmake=${product}\n")
EOF
        /usr/bin/cmake -DOUTPUT_PATH="$WORK_DIR/result.txt" \
            -P "$WORK_DIR/workload.cmake"
        grep -Fxq 'cmake=42' "$WORK_DIR/result.txt"
        printf 'cmake:script-product-42\n'
        ;;
    pkg-config)
        mkdir -p "$WORK_DIR/pkgconfig"
        cat >"$WORK_DIR/pkgconfig/hermit-compat.pc" <<'EOF'
prefix=/opt/hermit-compat
exec_prefix=${prefix}
libdir=${exec_prefix}/lib
includedir=${prefix}/include

Name: hermit-compat
Description: Hermit pkg-config compatibility fixture
Version: 1.2.3
Libs: -L${libdir} -lhermit_compat
Cflags: -I${includedir} -DHERMIT_COMPAT=42
EOF
        export PKG_CONFIG_PATH="$WORK_DIR/pkgconfig"
        version=$(/usr/bin/pkg-config --modversion hermit-compat)
        flags=$(/usr/bin/pkg-config --cflags --libs hermit-compat)
        flags=${flags% }
        test "$version" = '1.2.3'
        test "$flags" = \
            '-I/opt/hermit-compat/include -DHERMIT_COMPAT=42 -L/opt/hermit-compat/lib -lhermit_compat'
        /usr/bin/pkg-config --atleast-version=1.2 hermit-compat
        printf 'pkg-config:%s:%s\n' "$version" "$flags"
        ;;
    m4)
        cat >"$WORK_DIR/input.m4" <<'EOF'
define(`PRODUCT', `eval($1 * $2)')dnl
product=PRODUCT(6, 7)
EOF
        /usr/bin/m4 "$WORK_DIR/input.m4" >"$WORK_DIR/output.txt"
        grep -Fxq 'product=42' "$WORK_DIR/output.txt"
        printf 'm4:product-42\n'
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
        # These freshly-created inputs intentionally precede the missing
        # target. Treat them as old after that first build so GNU make does not
        # emit a host-timing-sensitive clock-skew diagnostic.
        make --no-print-directory -s -C "$WORK_DIR" \
            --old-file=Makefile --old-file=input.txt
        make --no-print-directory -q -C "$WORK_DIR" \
            --old-file=Makefile --old-file=input.txt --old-file=result.txt
        IFS= read -r result <"$WORK_DIR/result.txt"
        test "$result" = 'make:42'
        printf '%s\n' "$result"
        ;;
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#697): Review the fixture-backed system utility workloads.
    ip)
        output=$(/usr/sbin/ip -o -4 addr show dev lo)
        [[ $output == *" inet 127.0.0.1/8 "* ]]
        [[ $output == *" scope host lo"* ]]
        printf 'ip:loopback-ipv4-ok\n'
        ;;
    ss)
        if [[ -x /usr/sbin/ss ]]; then
            readonly SS=/usr/sbin/ss
        else
            readonly SS=/usr/bin/ss
        fi
        output=$("$SS" -H -ltn 'sport = :0')
        test -z "$output"
        printf 'ss:no-port-zero-listener\n'
        ;;
    lscpu)
        readonly root="$WORK_DIR/sysroot"
        mkdir -p "$root/proc" \
            "$root/sys/devices/system/cpu/cpu0/topology"
        cat >"$root/proc/cpuinfo" <<'EOF'
processor : 0
vendor_id : GenuineIntel
cpu family : 6
model : 85
model name : Hermit Virtual CPU
stepping : 7
cpu MHz : 1000.000
cache size : 1024 KB
physical id : 0
siblings : 1
core id : 0
cpu cores : 1
flags : fpu
EOF
        for file in online possible present; do
            printf '0\n' >"$root/sys/devices/system/cpu/$file"
        done
        printf '0\n' >"$root/sys/devices/system/cpu/cpu0/topology/core_id"
        printf '0\n' \
            >"$root/sys/devices/system/cpu/cpu0/topology/physical_package_id"
        output=$(/usr/bin/lscpu --sysroot "$root" -p=CPU,ONLINE)
        [[ $output == *"# CPU,Online"* ]]
        [[ $output == *"0,Y"* ]]
        printf 'lscpu:cpu0-online\n'
        ;;
    lsof)
        printf 'lsof-fixture\n' >"$WORK_DIR/input.txt"
        exec 9<"$WORK_DIR/input.txt"
        output=$(/usr/bin/lsof -O -w -p $$ -a -d 9 -Ffn)
        [[ $output == *f9* ]]
        [[ $output == *"$WORK_DIR/input.txt"* ]]
        printf 'lsof:fd9-ok\n'
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
        cat >"$WORK_DIR/compat.s" <<'EOF'
    .text
    .globl compat_add
    .type compat_add,@function
compat_add:
    lea (%rdi,%rsi), %eax
    ret
    .size compat_add, .-compat_add
    .section .note.GNU-stack,"",@progbits
EOF
        /usr/bin/as --64 "$WORK_DIR/compat.s" -o "$WORK_DIR/compat.o"
        /usr/bin/ld -shared --build-id=none \
            -o "$WORK_DIR/libcompat.so" "$WORK_DIR/compat.o"
        /usr/bin/readelf -h "$WORK_DIR/libcompat.so" \
            | grep -q 'DYN (Shared object file)'
        /usr/bin/python3 - "$WORK_DIR/libcompat.so" <<'PY'
import ctypes
import sys

library = ctypes.CDLL(sys.argv[1])
library.compat_add.argtypes = (ctypes.c_int, ctypes.c_int)
library.compat_add.restype = ctypes.c_int
result = library.compat_add(19, 23)
if result != 42:
    raise SystemExit(f"compat_add returned {result}, expected 42")
PY
        printf 'ld:shared-compat-add-42\n'
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
        cp "$FIXTURE_ROOT/binutils/with-symbols.o" "$WORK_DIR/compat.o"
        /usr/bin/ar crDS "$WORK_DIR/libcompat.a" "$WORK_DIR/compat.o"
        /usr/bin/ranlib -D "$WORK_DIR/libcompat.a"
        /usr/bin/nm -s "$WORK_DIR/libcompat.a" >"$WORK_DIR/archive-index.txt"
        grep -qx 'compat_line in compat.o' "$WORK_DIR/archive-index.txt"
        printf 'ranlib:indexed-fixture-archive\n'
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
    # AUTONOMOUS-BOT-IMPLEMENTED
    # TODO-HUMAN-REVIEW(#700): Review the expanded miscellaneous workloads.
    seq)
        output=$(/usr/bin/seq 2 3 20 | /usr/bin/paste -sd, -)
        test "$output" = '2,5,8,11,14,17,20'
        printf 'seq:stepped-range-ok\n'
        ;;
    find)
        readonly tree="$WORK_DIR/tree"
        mkdir -p "$tree/a" "$tree/b/nested"
        printf 'alpha\n' >"$tree/a/alpha.txt"
        printf 'beta\n' >"$tree/b/beta.log"
        printf 'gamma\n' >"$tree/b/nested/gamma.txt"
        output=$(/usr/bin/find "$tree" -type f -name '*.txt' -printf '%P\n' |
            /usr/bin/sort)
        test "$output" = $'a/alpha.txt\nb/nested/gamma.txt'
        printf 'find:recursive-filter-ok\n'
        ;;
    env)
        output=$(/usr/bin/env -i ALPHA=6 BETA=7 /usr/bin/printenv ALPHA BETA)
        test "$output" = $'6\n7'
        printf 'env:clean-two-vars-ok\n'
        ;;
    factor)
        output=$(/usr/bin/factor 360 97)
        test "$output" = $'360: 2 2 2 3 3 5\n97: 97'
        printf 'factor:composite-and-prime-ok\n'
        ;;
    xargs)
        output=$(printf '6\n7\n' | /usr/bin/xargs -n1 /usr/bin/expr 6 '*')
        test "$output" = $'36\n42'
        printf 'xargs:two-products-ok\n'
        ;;
    time)
        /usr/bin/time -f 'exit=%x maxrss=%M' -o "$WORK_DIR/timing.txt" \
            /bin/sh -c "/bin/echo time-command-ok >$WORK_DIR/command.txt"
        IFS= read -r command_output <"$WORK_DIR/command.txt"
        IFS= read -r timing <"$WORK_DIR/timing.txt"
        test "$command_output" = 'time-command-ok'
        test "$timing" = 'exit=0 maxrss=0'
        printf 'time:exit-and-rusage-ok\n'
        ;;
    shuf)
        output=$(printf 'alpha\nbeta\ngamma\ndelta\n' |
            /usr/bin/shuf | /usr/bin/sort)
        test "$output" = $'alpha\nbeta\ndelta\ngamma'
        printf 'shuf:permutation-ok\n'
        ;;
    *)
        echo "unknown real compatibility workload: $PROGRAM" >&2
        exit 2
        ;;
esac
