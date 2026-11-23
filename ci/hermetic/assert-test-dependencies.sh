#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Assert the runner and system-utils family tools carried by a composed
# test-node image (mini + runner + crypto stack). Test-running nodes
# only: build nodes keep assert-build-dependencies.sh unchanged.
# The list derives from programs the qualifying system-utils run
# actually executed plus runner tools, not from manifest tool tokens
# alone (ssh-keygen-ed25519 declares none). Keep in sync with
# groups.nix by re-qualification, never by shrinking to go green.
set -euo pipefail

test_dependencies=(
    awk bash cat chmod cp date dirname du env find grep head ls mkdir mktemp
    mv printf readelf rm sha256sum sh sleep sort stat tail tr true uname wc
    python3 git
    openssl ssh-keygen uuidgen mcookie hexdump ps uptime shuf
    iostat ip lsmod mpstat numactl pidstat sar ss
)

# Absolute guest paths this family executes. miniGuestPathLinks omits
# a link silently when its provider is absent; this list turns a
# dropped link into one named admission refusal, not red cells.
# Only paths ci/hermetic/guest-paths.txt actually requires for this
# family (openssl/ssh-keygen are PATH guests, covered by tools above).
family_guest_paths=(
    /bin/bash /bin/cat /bin/date /bin/du /bin/find /bin/printf /bin/sh /bin/true
    /usr/bin/bash /usr/bin/date /usr/bin/du /usr/bin/env /usr/bin/find /usr/bin/printf /usr/bin/true
    /usr/bin/uuidgen /usr/bin/hexdump
)

runner_libraries=(libunwind-ptrace.so.0 libunwind-x86_64.so.8 libc.so.6 libm.so.6 libgcc_s.so.1)

check_tools() {
    local search_path=${1:-$PATH}
    local missing=()
    local tool
    for tool in "${test_dependencies[@]}"; do
        # type -P forces a PATH file: bash builtins (printf, true) must
        # not satisfy a check for the image's external binaries.
        if ! PATH="$search_path" type -P "$tool" >/dev/null 2>&1; then
            missing+=("$tool")
        fi
    done
    if ((${#missing[@]})); then
        printf 'assert-test-dependencies: missing required test tools (%d): %s\n' \
            "${#missing[@]}" "${missing[*]}" >&2
        return 2
    fi
    printf 'assert-test-dependencies: %d test tools present\n' "${#test_dependencies[@]}"
}

check_guest_paths() {
    local root=${1:-}
    local missing=()
    local path
    for path in "${family_guest_paths[@]}"; do
        if [ ! -x "$root$path" ]; then
            missing+=("$path")
        fi
    done
    if ((${#missing[@]})); then
        printf 'assert-test-dependencies: missing family guest paths (%d): %s\n' \
            "${#missing[@]}" "${missing[*]}" >&2
        return 2
    fi
    printf 'assert-test-dependencies: %d family guest paths present\n' "${#family_guest_paths[@]}"
}

check_runner_libraries() {
    # With an argument, resolve NEEDED through that binary's RUNPATH.
    # Without one, fall back to the library search below (self-test).
    local hermit_bin=${1:-}
    if [ -n "$hermit_bin" ] && [ -x "$hermit_bin" ] && command -v readelf >/dev/null 2>&1; then
        local runpath needed dir lib found
        runpath=$(readelf -d "$hermit_bin" 2>/dev/null | sed -n 's/.*RUNPATH.*\[\(.*\)\]/\1/p')
        needed=$(readelf -d "$hermit_bin" 2>/dev/null | sed -n 's/.*NEEDED.*\[\(.*\)\]/\1/p')
        for lib in $needed; do
            found=0
            local IFS=:
            for dir in $runpath /lib /usr/lib; do
                if [ -e "$dir/$lib" ]; then found=1; break; fi
            done
            unset IFS
            if [ "$found" != 1 ]; then
                printf 'assert-test-dependencies: runner library %s not on RUNPATH of %s\n' "$lib" "$hermit_bin" >&2
                return 2
            fi
        done
        return 0
    fi
    local library dir found
    for library in "${runner_libraries[@]}"; do
        found=0
        for dir in /lib /usr/lib /nix/store/*/lib; do
            if [ -e "$dir/$library" ]; then found=1; break; fi
        done
        if [ "$found" != 1 ]; then
            printf 'assert-test-dependencies: missing runner library: %s\n' "$library" >&2
            return 2
        fi
    done
}

self_test() {
    local root tool rc output guest_root path
    root=$(mktemp -d)
    trap 'rm -rf "$root"' RETURN
    mkdir -p "$root/bin" "$root/lib"
    for tool in "${test_dependencies[@]}"; do
        printf '#!/bin/sh\nexit 0\n' >"$root/bin/$tool"
        chmod +x "$root/bin/$tool"
    done
    if ! output=$(check_tools "$root/bin" 2>&1); then
        echo "assert-test-dependencies --self-test: complete fixture was rejected: $output" >&2
        return 1
    fi
    case "$output" in
        *"${#test_dependencies[@]} test tools present"*) ;;
        *) echo "assert-test-dependencies --self-test: success count was not reported" >&2; return 1 ;;
    esac
    for tool in "${test_dependencies[@]}"; do
        mv "$root/bin/$tool" "$root/bin/$tool.off"
        set +e
        output=$(check_tools "$root/bin" 2>&1)
        rc=$?
        set -e
        mv "$root/bin/$tool.off" "$root/bin/$tool"
        if [ "$rc" != 2 ]; then
            echo "assert-test-dependencies --self-test: missing $tool returned $rc, expected 2" >&2
            return 1
        fi
        case "$output" in
            *"$tool"*) ;;
            *) echo "assert-test-dependencies --self-test: missing $tool was not named" >&2; return 1 ;;
        esac
    done
    guest_root="$root/guest"
    for path in "${family_guest_paths[@]}"; do
        mkdir -p "$guest_root$(dirname "$path")"
        printf '#!/bin/sh\nexit 0\n' >"$guest_root$path"
        chmod +x "$guest_root$path"
    done
    check_guest_paths "$guest_root"
    rm "$guest_root/usr/bin/du"
    set +e
    output=$(check_guest_paths "$guest_root" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: dropped guest path returned $rc, expected 2: $output" >&2
        return 1
    fi
    echo "assert-test-dependencies --self-test: OK"
}

if [ "${1:-}" = --self-test ]; then
    self_test
    exit 0
fi

check_tools
check_guest_paths ""
# Hermit artifact RUNPATH check when the staged binary is visible.
hermit_bin=""
for candidate in /src/target/ci/hermit-e2e-artifacts/*/hermit; do
    if [ -x "$candidate" ]; then hermit_bin=$candidate; break; fi
done
check_runner_libraries "$hermit_bin"
printf 'assert-test-dependencies: OK -- %d test tools, %d guest paths and runner libraries present\n' \
    "${#test_dependencies[@]}" "${#family_guest_paths[@]}"
