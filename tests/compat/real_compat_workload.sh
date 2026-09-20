#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Functional compatibility workloads for scripts/validate.rs's strict L2 matrix.

set -euo pipefail

if (($# != 1)); then
    echo "usage: $0 PROGRAM" >&2
    exit 2
fi

readonly PROGRAM=$1
readonly FIXTURE_ROOT=${REAL_COMPAT_FIXTURES:-/tmp/hermit-real-compat-fixtures}
# Hosted strict verification mounts a fresh tmpfs at /test for each physical
# run and executes this script there. Allocate below that per-run filesystem so
# inode-bearing getdents64 buffers and /proc/self/maps observations start from
# the same baseline even while a -j2 peer is active. Other callers retain /tmp.
if [[ $PWD == /test ]]; then
    WORK_ROOT=$PWD
else
    WORK_ROOT=/tmp
fi
readonly WORK_ROOT
# A real allocation, not a name derived from the program. The old
# "/tmp/hermit-real-compat-$PROGRAM" was the same directory for every concurrent
# run of one program, and the two lines below made that MUTUALLY DESTRUCTIVE
# rather than merely shared: the second run's `rm -rf` deleted the first run's
# tree mid-flight, and whichever finished first deleted it again from its EXIT
# trap. Measured over ten concurrent pairs of `curl-localhost`: seven pairs had
# at least one side fail. `mktemp -d` creates the directory itself, so the
# `rm -rf`/`mkdir` that opened the window are gone with it, and the trap now
# only ever removes this run's own tree.
WORK_DIR="$(mktemp -d "$WORK_ROOT/hermit-real-compat-$PROGRAM.XXXXXXXX")"
readonly WORK_DIR
export LC_ALL=C
export TZ=UTC
umask 022

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

function published_localhost_port {
    local port_file=$1
    local -a lines=()
    local port

    mapfile -t lines <"$port_file"
    if ((${#lines[@]} != 1)); then
        printf 'expected exactly one published localhost port, found %s\n' \
            "${#lines[@]}" >&2
        return 1
    fi
    port=${lines[0]}
    if [[ ! $port =~ ^[0-9]+$ ]] || ((${#port} > 5)); then
        printf 'published localhost port is invalid: %q\n' "$port" >&2
        return 1
    fi
    port=$((10#$port))
    if ((port == 0 || port > 65535)); then
        printf 'published localhost port is invalid: %q\n' "${lines[0]}" >&2
        return 1
    fi
    printf '%s\n' "$port"
}

function wait_for_published_localhost_port {
    local port_file=$1
    local server_pid=$2
    local attempts=${3:-100}
    local attempt

    for ((attempt = 0; attempt < attempts; attempt++)); do
        if [[ -s $port_file ]]; then
            published_localhost_port "$port_file"
            return
        fi
        if ! kill -0 "$server_pid" 2>/dev/null; then
            printf 'localhost server exited before publishing its port\n' >&2
            return 1
        fi
        sleep 0.01
    done
    printf 'timed out waiting for localhost server to publish its port\n' >&2
    return 1
}

function require_live_localhost_server {
    local server_pid=$1
    local server_port=$2

    if ! kill -0 "$server_pid" 2>/dev/null; then
        printf 'localhost server exited after publishing port %s\n' "$server_port" >&2
        return 1
    fi
}

function test_published_localhost_port {
    local got
    local failures=0
    local name name_value value

    printf '32768\n' >"$WORK_DIR/valid.port"
    got=$(published_localhost_port "$WORK_DIR/valid.port")
    if [[ $got != 32768 ]]; then
        printf 'localhost-port self-test: expected 32768, got %q\n' "$got" >&2
        failures=$((failures + 1))
    fi

    : >"$WORK_DIR/missing.port"
    if published_localhost_port "$WORK_DIR/missing.port" \
        >"$WORK_DIR/missing.out" 2>"$WORK_DIR/missing.err"; then
        printf 'localhost-port self-test: missing port was accepted\n' >&2
        failures=$((failures + 1))
    elif ! grep -Fq 'found 0' "$WORK_DIR/missing.err"; then
        printf 'localhost-port self-test: missing-port diagnostic was not explicit\n' >&2
        failures=$((failures + 1))
    fi

    printf '32768\n32769\n' >"$WORK_DIR/multiple.port"
    if published_localhost_port "$WORK_DIR/multiple.port" \
        >"$WORK_DIR/ambiguous.out" 2>"$WORK_DIR/ambiguous.err"; then
        printf 'localhost-port self-test: multiple ports were accepted\n' >&2
        failures=$((failures + 1))
    elif ! grep -Fq 'found 2' "$WORK_DIR/ambiguous.err"; then
        printf 'localhost-port self-test: multiple-port diagnostic was not explicit\n' >&2
        failures=$((failures + 1))
    fi

    for name_value in \
        'zero:0' \
        'nondecimal:not-a-port' \
        'too-large:65536' \
        'overflow:18446744073709551617'; do
        name=${name_value%%:*}
        value=${name_value#*:}
        printf '%s\n' "$value" >"$WORK_DIR/$name.port"
        if published_localhost_port "$WORK_DIR/$name.port" \
            >"$WORK_DIR/$name.out" 2>"$WORK_DIR/$name.err"; then
            printf 'localhost-port self-test: %s port was accepted\n' "$name" >&2
            failures=$((failures + 1))
        elif ! grep -Fq 'published localhost port is invalid' "$WORK_DIR/$name.err"; then
            printf 'localhost-port self-test: %s diagnostic was not explicit\n' "$name" >&2
            failures=$((failures + 1))
        fi
    done

    if wait_for_published_localhost_port "$WORK_DIR/never-created.port" "$$" 1 \
        >"$WORK_DIR/timeout.out" 2>"$WORK_DIR/timeout.err"; then
        printf 'localhost-port self-test: timeout was accepted\n' >&2
        failures=$((failures + 1))
    elif ! grep -Fq 'timed out waiting' "$WORK_DIR/timeout.err"; then
        printf 'localhost-port self-test: timeout diagnostic was not explicit\n' >&2
        failures=$((failures + 1))
    fi

    (:) &
    local dead_pid=$!
    wait "$dead_pid"
    if wait_for_published_localhost_port "$WORK_DIR/dead-before.port" "$dead_pid" 1 \
        >"$WORK_DIR/dead-before.out" 2>"$WORK_DIR/dead-before.err"; then
        printf 'localhost-port self-test: dead server before publication was accepted\n' >&2
        failures=$((failures + 1))
    elif ! grep -Fq 'exited before publishing' "$WORK_DIR/dead-before.err"; then
        printf 'localhost-port self-test: dead-before diagnostic was not explicit\n' >&2
        failures=$((failures + 1))
    fi

    printf '32768\n' >"$WORK_DIR/dead-after.port"
    if require_live_localhost_server "$dead_pid" 32768 \
        >"$WORK_DIR/dead-after.out" 2>"$WORK_DIR/dead-after.err"; then
        printf 'localhost-port self-test: dead server after publication was accepted\n' >&2
        failures=$((failures + 1))
    elif ! grep -Fq 'exited after publishing port 32768' "$WORK_DIR/dead-after.err"; then
        printf 'localhost-port self-test: dead-after diagnostic was not explicit\n' >&2
        failures=$((failures + 1))
    fi

    if ((failures != 0)); then
        return 1
    fi
    printf 'localhost-port self-test: PASS\n'
}

function fetch_localhost_payload {
    (
        local client=$1
        local response_bytes
        local server_bin="$FIXTURE_ROOT/localhost-http-server"
        local port_file="$WORK_DIR/server.port"
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

        if [[ ! -x $server_bin ]]; then
            printf 'localhost server fixture is missing or not executable: %s\n' "$server_bin" >&2
            return 1
        fi

        # Never inspect the socket table inside the measured verification run.
        # RUN1562's `ss -p` setup exited before wget/curl because guest PID
        # attribution was absent; comparing socket tables let the clients run but
        # made host netlink bytes diverge. Historical `--verify` could also miss
        # content-only divergence. The process that binds port 0 therefore owns
        # getsockname(2), publishes its own decimal port, and serves the response.
        "$server_bin" "$port_file" "$WORK_DIR/response.http" \
            >"$WORK_DIR/server.log" 2>&1 &
        server_pid=$!
        local server_port=''
        if ! server_port=$(wait_for_published_localhost_port "$port_file" "$server_pid"); then
            kill "$server_pid" 2>/dev/null || true
            wait "$server_pid" 2>/dev/null || true
            cat "$WORK_DIR/server.log" >&2
            return 1
        fi
        if ! require_live_localhost_server "$server_pid" "$server_port"; then
            wait "$server_pid" 2>/dev/null || true
            server_pid=
            cat "$WORK_DIR/server.log" >&2
            return 1
        fi
        if [[ $client == wget ]]; then
            /usr/bin/wget --quiet \
                --output-document="$WORK_DIR/output/payload.txt" \
                http://127.0.0.1:$server_port/payload.txt || status=$?
        else
            /usr/bin/curl --fail --silent --show-error \
                --output "$WORK_DIR/output/payload.txt" \
                http://127.0.0.1:$server_port/payload.txt || status=$?
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
    --self-test-localhost-port)
        test_published_localhost_port
        ;;
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
        CARGO="$FIXTURE_ROOT/toolchain/cargo"
        RUSTC="$FIXTURE_ROOT/toolchain/rustc"
        test -x "$CARGO"
        test -x "$RUSTC"
        RUSTC="$RUSTC" "$CARGO" metadata --offline --format-version 1 --no-deps \
            --manifest-path "$WORK_DIR/Cargo.toml" >"$WORK_DIR/metadata.json"
        RUSTC="$RUSTC" "$CARGO" package --offline --allow-dirty --list \
            --manifest-path "$WORK_DIR/Cargo.toml" >"$WORK_DIR/package-files.txt"
        grep -q '"name":"hermit-real-compat"' "$WORK_DIR/metadata.json"
        grep -qx 'src/lib.rs' "$WORK_DIR/package-files.txt"
        printf 'cargo:metadata-and-package-list\n'
        ;;
    df)
        # Coreutils df reads /proc/self/mountinfo before statfs(2). Feed that
        # read from the immutable fixture through an inherited descriptor so
        # Hermit's per-attempt private mount roots cannot contaminate the
        # strict comparison. The marker belongs to this invocation's private
        # work directory, so concurrent df probes cannot truncate each other.
        output=$(HERMIT_LSOF_MOUNTS_FD=0 \
            HERMIT_LSOF_REDIRECT_MARKER="$WORK_DIR/df.redirected" \
            LD_PRELOAD="$FIXTURE_ROOT/lsof/libmount_redirect.so" \
            /usr/bin/df -P / <"$FIXTURE_ROOT/df/mountinfo")
        [[ $(cat "$WORK_DIR/df.redirected") == fixed-mount-fd ]]
        printf '%s\n' "$output"
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
        // The work directory arrives as argv rather than being spelled out: the
        // heredoc is quoted, so $WORK_DIR cannot be interpolated here, and the
        // literal it used to hold silently stopped tracking the real directory
        // the moment that stopped being a fixed name.
        Path path = Paths.get(args[0], "data.txt");
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
            -cp "$WORK_DIR" Compat "$WORK_DIR"
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
    # A standalone NETLINK_ROUTE probe. The `ip` and `ss` rows above reach
    # netlink incidentally but assert nothing about the reply, so before this
    # row no corpus cell would have failed if detcore stopped normalizing
    # netlink entirely. Runs under the default `--network=local`, i.e. one
    # isolated loopback interface.
    #
    # The digest deliberately covers the WHOLE reply except two named fields.
    # `IFLA_INET6_CACHEINFO.tstamp` is host uptime in USER_HZ and
    # `.reachable_time` is a per-interface random draw the kernel is required to
    # randomize (RFC 4861 6.3.2); both reach the guest unnormalized today. They
    # are located by walking the attribute tree rather than by a fixed offset,
    # and they are 8 bytes of 1488 -- the other 1480 are covered, so a NEW leak
    # anywhere else in the reply changes the digest and fails this row.
    #
    # HERMIT_NETLINK_UNMASK=1 drops the mask. It can only ever make this row
    # FAIL, never pass, so it cannot be used to weaken the check; it exists so
    # the sensitivity of the digest stays reproducible. Masked, four runs agree;
    # unmasked, four runs produced four distinct digests.
    netlink-route)
        /usr/bin/python3 -I -B - <<'PY'
import hashlib, os, socket, struct

NETLINK_ROUTE, RTM_GETLINK, RTM_NEWLINK = 0, 18, 16
NLM_F_REQUEST, NLM_F_DUMP, NLMSG_DONE = 0x001, 0x300, 3
IFLA_IFNAME, IFLA_AF_SPEC, IFLA_INET6_CACHEINFO = 3, 26, 5
UNMASK = os.environ.get("HERMIT_NETLINK_UNMASK") == "1"

sock = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, NETLINK_ROUTE)
sock.bind((0, 0))
sock.send(
    struct.pack("=IHHII", 20, RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 1, 0)
    + struct.pack("=Bxxx", socket.AF_PACKET)
)
blob = b""
while True:
    chunk = sock.recv(65536)
    blob += chunk
    if len(chunk) >= 6 and struct.unpack("=H", chunk[4:6])[0] == NLMSG_DONE:
        break
    if len(chunk) < 20:
        break
sock.close()

def attrs(buf, start, end):
    off = start
    while off + 4 <= end:
        rta_len, rta_type = struct.unpack("<HH", buf[off:off + 4])
        if rta_len < 4 or off + rta_len > end:
            return
        yield rta_type, off + 4, off + rta_len
        off += (rta_len + 3) & ~3

def walk(buf):
    off = 0
    while off + 16 <= len(buf):
        nlmsg_len, nlmsg_type = struct.unpack("<IH", buf[off:off + 6])
        if nlmsg_len < 16 or off + nlmsg_len > len(buf):
            return
        yield off, nlmsg_len, nlmsg_type
        off += (nlmsg_len + 3) & ~3

masked_bytes = bytearray(blob)
masked_count = 0
for off, nlmsg_len, nlmsg_type in walk(blob):
    if nlmsg_type != RTM_NEWLINK:
        continue
    for t, ps, pe in attrs(blob, off + 32, off + nlmsg_len):
        if t != IFLA_AF_SPEC:
            continue
        for fam, fs, fe in attrs(blob, ps, pe):
            if fam != socket.AF_INET6:
                continue
            for a, s2, e2 in attrs(blob, fs, fe):
                if a == IFLA_INET6_CACHEINFO and e2 - s2 >= 16:
                    for b in range(s2 + 4, s2 + 12):
                        masked_bytes[b] = 0
                    masked_count += 1

lines, messages = [], 0
for off, nlmsg_len, nlmsg_type in walk(blob):
    messages += 1
    if nlmsg_type != RTM_NEWLINK:
        continue
    _fam, _pad, ifi_type, ifi_index, ifi_flags, _chg = struct.unpack(
        "<BBHiII", blob[off + 16:off + 32]
    )
    name, inventory = "?", []
    for t, ps, pe in attrs(blob, off + 32, off + nlmsg_len):
        inventory.append("%d:%d" % (t, pe - ps))
        if t == IFLA_IFNAME:
            name = blob[ps:pe].rstrip(b"\0").decode()
    lines.append(
        "netlink-route:link name=%s index=%d type=%d flags=0x%08x attrs=%s"
        % (name, ifi_index, ifi_type, ifi_flags, ",".join(inventory))
    )

# Fail closed. An empty or link-less dump would make the digest trivially
# reproducible and certify nothing.
if messages == 0 or not lines:
    raise SystemExit("netlink-route: RTM_GETLINK returned no link messages")
if masked_count != 1:
    raise SystemExit(
        "netlink-route: expected exactly 1 IFLA_INET6_CACHEINFO, found %d" % masked_count
    )

body = blob if UNMASK else bytes(masked_bytes)
for line in lines:
    print(line)
print("netlink-route:messages=%d bytes=%d masked_fields=%d" % (messages, len(blob), masked_count))
print("netlink-route:digest=%s" % hashlib.sha256(body).hexdigest())
PY
        ;;
    # A standalone NETLINK_SOCK_DIAG probe covering EVERY kernel receive
    # syscall that can retrieve a netlink reply, not just one of them.
    #
    # This is the first end-to-end exercise of detcore/src/sock_diag.rs against a
    # real kernel reply -- its zeroing had only ever run against synthetic
    # buffers in its own unit tests. The corpus `ss` row above opens the same
    # protocol but its every reply is a 20-byte NLMSG_DONE carrying zero diag
    # messages, so it cannot reach the zeroing. Runs under the default
    # `--network=local`.
    #
    # Why all five: the sanitizer was originally wired into recvmsg alone, and
    # read, readv, recvfrom and recvmmsg reached the same dump without it, so a
    # guest could skip determinization just by choosing a different syscall --
    # `socket.recv()` was enough, since glibc lowers recv(2) to recvfrom and
    # there is no `recv` syscall on x86_64. Measured before the fix: recvmsg
    # returned nonzero_inodes=0 while the other four returned 5. This row exists
    # so that cannot regress silently on any one path.
    #
    # preadv/preadv2/pread64 are deliberately absent: a socket is not seekable,
    # so the kernel refuses them with ESPIPE before any data moves. Measured, not
    # assumed.
    #
    # Sensitivity is not a test-only knob: `--no-virtualize-metadata` is the
    # production gate at detcore/src/syscalls/io.rs. With it, all five paths
    # return raw host inode numbers and this row exits 1.
    netlink-sock-diag)
        /usr/bin/python3 -I -B - <<'PY'
import ctypes, socket, struct

libc = ctypes.CDLL("libc.so.6", use_errno=True)
NR = {"read": 0, "readv": 19, "recvfrom": 45, "recvmsg": 47, "recvmmsg": 299}

NETLINK_SOCK_DIAG, SOCK_DIAG_BY_FAMILY = 4, 20
NLM_F_REQUEST, NLM_F_DUMP = 0x001, 0x300
UDIAG_SHOW_NAME, UNIX_DIAG_INO_OFFSET = 0x01, 4
BUFSZ = 65536


class iovec(ctypes.Structure):
    _fields_ = [("iov_base", ctypes.c_void_p), ("iov_len", ctypes.c_size_t)]


class msghdr(ctypes.Structure):
    _fields_ = [
        ("msg_name", ctypes.c_void_p), ("msg_namelen", ctypes.c_uint32),
        ("msg_iov", ctypes.POINTER(iovec)), ("msg_iovlen", ctypes.c_size_t),
        ("msg_control", ctypes.c_void_p), ("msg_controllen", ctypes.c_size_t),
        ("msg_flags", ctypes.c_int),
    ]


class mmsghdr(ctypes.Structure):
    _fields_ = [("msg_hdr", msghdr), ("msg_len", ctypes.c_uint32)]


# Bind listeners so every dump is POPULATED. Abstract namespace: no filesystem
# artifact to clean up and no path to vary.
held = []
for index in range(3):
    unix_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    unix_sock.bind("\0hermit-compat-sockdiag-%d" % index)
    unix_sock.listen(1)
    held.append(unix_sock)


def open_dump():
    """A bound socket-diag socket with a UNIX_DIAG dump already requested."""
    nl = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, NETLINK_SOCK_DIAG)
    nl.bind((0, 0))
    nl.send(
        struct.pack("=IHHII", 40, SOCK_DIAG_BY_FAMILY, NLM_F_REQUEST | NLM_F_DUMP, 1, 0)
        + struct.pack("=BBHIIIII", socket.AF_UNIX, 0, 0, 0xFFFFFFFF, 0, UDIAG_SHOW_NAME, 0, 0)
    )
    return nl


def syscall(name, *args):
    ctypes.set_errno(0)
    return libc.syscall(ctypes.c_long(NR[name]), *args), ctypes.get_errno()


def one_iov(buf):
    iov = (iovec * 1)()
    iov[0].iov_base = ctypes.cast(buf, ctypes.c_void_p)
    iov[0].iov_len = BUFSZ
    return iov


def blank_msghdr(buf):
    header = msghdr()
    ctypes.memset(ctypes.byref(header), 0, ctypes.sizeof(header))
    header.msg_iov = one_iov(buf)
    header.msg_iovlen = 1
    return header


def via_read(fd, buf):
    return syscall("read", ctypes.c_int(fd), buf, ctypes.c_size_t(BUFSZ))


def via_readv(fd, buf):
    return syscall("readv", ctypes.c_int(fd), one_iov(buf), ctypes.c_int(1))


def via_recvfrom(fd, buf):
    return syscall("recvfrom", ctypes.c_int(fd), buf, ctypes.c_size_t(BUFSZ),
                   ctypes.c_int(0), None, None)


def via_recvmsg(fd, buf):
    header = blank_msghdr(buf)
    return syscall("recvmsg", ctypes.c_int(fd), ctypes.byref(header), ctypes.c_int(0))


def via_recvmmsg(fd, buf):
    batch = (mmsghdr * 1)()
    ctypes.memset(ctypes.byref(batch), 0, ctypes.sizeof(batch))
    batch[0].msg_hdr = blank_msghdr(buf)
    rc, err = syscall("recvmmsg", ctypes.c_int(fd), batch, ctypes.c_uint(1),
                      ctypes.c_int(0), None)
    return (batch[0].msg_len if rc > 0 else rc), err


METHODS = [
    ("read", via_read), ("readv", via_readv), ("recvfrom", via_recvfrom),
    ("recvmsg", via_recvmsg), ("recvmmsg", via_recvmmsg),
]


def inodes_in(blob, size):
    found, off = [], 0
    while off + 16 <= size:
        nlmsg_len, nlmsg_type = struct.unpack("<IH", blob[off:off + 6])
        if nlmsg_len < 16 or off + nlmsg_len > size:
            break
        if nlmsg_type == SOCK_DIAG_BY_FAMILY:
            body = off + 16
            if body + UNIX_DIAG_INO_OFFSET + 4 <= off + nlmsg_len:
                found.append(struct.unpack(
                    "<I",
                    blob[body + UNIX_DIAG_INO_OFFSET:body + UNIX_DIAG_INO_OFFSET + 4],
                )[0])
        off += (nlmsg_len + 3) & ~3
    return found


for name, receive in METHODS:
    nl = open_dump()
    buf = ctypes.create_string_buffer(BUFSZ)
    count, err = receive(nl.fileno(), buf)
    nl.close()
    if count < 0:
        raise SystemExit("netlink-sock-diag: %s failed, errno %d" % (name, err))
    inodes = inodes_in(bytes(buf.raw), count)

    # Fail closed per path. Zero diag messages would make "every inode is zero"
    # vacuously true, which is the empty-corpus defect wearing a different hat.
    if len(inodes) < len(held):
        raise SystemExit(
            "netlink-sock-diag: %s returned %d diag message(s), expected at least %d"
            % (name, len(inodes), len(held))
        )

    # Assert, do not merely print. The COUNT of nonzero inodes is stable across
    # runs whether or not the sanitizer ran, so a printed count would be compared
    # equal by --verify and certify nothing. Only the values move.
    nonzero = [i for i in inodes if i != 0]
    if nonzero:
        raise SystemExit(
            "netlink-sock-diag: %s delivered %d of %d socket inode(s) unzeroed "
            "(first: %d); this receive path bypassed detcore/src/sock_diag.rs"
            % (name, len(nonzero), len(inodes), nonzero[0])
        )
    print("netlink-sock-diag:%s diag_messages=%d all_inodes_zeroed=yes"
          % (name, len(inodes)))
PY
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
        printf 'lsof-wrong-fd\n' >"$WORK_DIR/wrong-fd.txt"
        exec 8<"$WORK_DIR/wrong-fd.txt"
        exec 9<"$WORK_DIR/input.txt"

        # The preload library must refuse an inherited descriptor for the live
        # procfs table.  The selection below is otherwise the same qualifying
        # case as the final invocation, and no success marker may be emitted.
        live_proc_status=0
        HERMIT_LSOF_MOUNTS_FD=0 \
            HERMIT_LSOF_REDIRECT_MARKER="$WORK_DIR/live-proc.redirected" \
            LD_PRELOAD="$FIXTURE_ROOT/lsof/libmount_redirect.so" \
            /usr/bin/lsof -O -w -p $$ -a -d 9 -a -Ffn \
                -- "$WORK_DIR/input.txt" \
                </proc/mounts >"$WORK_DIR/live-proc.out" \
                2>"$WORK_DIR/live-proc.err" || live_proc_status=$?
        test "$live_proc_status" -eq 1
        test ! -s "$WORK_DIR/live-proc.out"
        [[ $(cat "$WORK_DIR/live-proc.err") == \
            'lsof: can'\''t fopen(/proc/mounts, "r"): Operation not permitted' ]]
        test ! -e "$WORK_DIR/live-proc.redirected"

        # Bracket the real selection in both directions.  The first invocation
        # must refuse a path that fd 9 does not hold; the second must report the
        # fixture descriptor.  Both must prove that lsof's unconditional
        # /proc/mounts read was served from the inherited fixed descriptor.
        refusal_status=0
        refusal_output=$(HERMIT_LSOF_MOUNTS_FD=0 \
            HERMIT_LSOF_REDIRECT_MARKER="$WORK_DIR/refusal.redirected" \
            LD_PRELOAD="$FIXTURE_ROOT/lsof/libmount_redirect.so" \
            /usr/bin/lsof -O -w -p $$ -a -d 9 -a -Ffn \
                -- "$WORK_DIR/wrong-fd.txt" \
                <"$FIXTURE_ROOT/lsof/mounts" \
                2>"$WORK_DIR/refusal.err") || refusal_status=$?
        test "$refusal_status" -eq 0
        test -z "$refusal_output"
        test ! -s "$WORK_DIR/refusal.err"
        [[ $(cat "$WORK_DIR/refusal.redirected") == fixed-mount-fd ]]

        output=$(HERMIT_LSOF_MOUNTS_FD=0 \
            HERMIT_LSOF_REDIRECT_MARKER="$WORK_DIR/accept.redirected" \
            LD_PRELOAD="$FIXTURE_ROOT/lsof/libmount_redirect.so" \
            /usr/bin/lsof -O -w -p $$ -a -d 9 -a -Ffn \
                -- "$WORK_DIR/input.txt" \
                <"$FIXTURE_ROOT/lsof/mounts" \
                2>"$WORK_DIR/accept.err")
        [[ $(cat "$WORK_DIR/accept.redirected") == fixed-mount-fd ]]
        test ! -s "$WORK_DIR/accept.err"
        expected=$(printf 'p%s\nf9\nn%s' "$$" "$WORK_DIR/input.txt")
        [[ $output == "$expected" ]]
        printf 'lsof:fixed-mount-fd=2/2:live-proc-fd-refused=1/1:wrong-path-refused=1/1:fd9-ok\n'
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
        /usr/bin/python3 -I -B - "$WORK_DIR/libcompat.so" <<'PY'
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
