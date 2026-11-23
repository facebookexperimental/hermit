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

# Print the runner binary the artifact pointer names: the one absolute
# bundle path in the pointer file, plus /hermit. This is the same source of
# truth the consumer (ci/run-with-hermit-e2e-artifact.sh) resolves, so the
# guard checks exactly the binary that will run. Missing or malformed
# pointers are named refusals, never a search for some other artifact.
pointer_runner() {
    local pointer=${1:-${HERMIT_E2E_ARTIFACT_POINTER:-/src/target/ci/hermit-e2e-artifact.path}}
    local bundle
    if [ ! -s "$pointer" ]; then
        printf 'assert-test-dependencies: artifact pointer missing or empty: %s\n' "$pointer" >&2
        return 2
    fi
    IFS= read -r bundle <"$pointer" || {
        printf 'assert-test-dependencies: cannot read artifact pointer: %s\n' "$pointer" >&2
        return 2
    }
    if [ -z "$bundle" ] || [ "${bundle#/}" = "$bundle" ] || [ "$(wc -l <"$pointer")" != 1 ]; then
        printf 'assert-test-dependencies: artifact pointer must contain one absolute path: %s\n' "$pointer" >&2
        return 2
    fi
    printf '%s\n' "$bundle/hermit"
}

check_runner_libraries() {
    # The argument is the runner binary; the main block always passes the
    # one the artifact pointer names. Every failure to inspect it is a
    # named refusal: there is no name-only fallback that could admit a
    # runner whose RUNPATH cannot resolve.
    local hermit_bin=${1:-}
    if [ -z "$hermit_bin" ]; then
        printf 'assert-test-dependencies: runner binary path not supplied\n' >&2
        return 2
    fi
    if [ ! -x "$hermit_bin" ]; then
        printf 'assert-test-dependencies: runner binary missing or not executable: %s\n' "$hermit_bin" >&2
        return 2
    fi
    if ! command -v readelf >/dev/null 2>&1; then
        printf 'assert-test-dependencies: readelf required to check RUNPATH of %s\n' "$hermit_bin" >&2
        return 2
    fi
    local dynamic runpath needed dir lib found
    dynamic=$(readelf -d "$hermit_bin" 2>/dev/null) || {
        printf 'assert-test-dependencies: cannot read dynamic section of runner binary: %s\n' "$hermit_bin" >&2
        return 2
    }
    runpath=$(sed -n 's/.*RUNPATH.*\[\(.*\)\]/\1/p' <<<"$dynamic")
    needed=$(sed -n 's/.*NEEDED.*\[\(.*\)\]/\1/p' <<<"$dynamic")
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
}

# The production wiring, in one function so the self-test can prove the
# main block checks exactly the runner the pointer names.
check_pointer_runner() {
    local hermit_bin
    hermit_bin=$(pointer_runner "$@") || return 2
    check_runner_libraries "$hermit_bin"
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
    set +e
    output=$(check_runner_libraries "$root/bin/no-such-hermit" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: nonexecutable runner path returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"runner binary missing or not executable: $root/bin/no-such-hermit"*) ;;
        *) echo "assert-test-dependencies --self-test: nonexecutable runner path was not named: $output" >&2; return 1 ;;
    esac
    # An existing file without the x bit is refused the same way.
    chmod -x "$root/bin/true"
    set +e
    output=$(check_runner_libraries "$root/bin/true" 2>&1)
    rc=$?
    set -e
    chmod +x "$root/bin/true"
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: mode-644 runner returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"runner binary missing or not executable: $root/bin/true"*) ;;
        *) echo "assert-test-dependencies --self-test: mode-644 runner was not named: $output" >&2; return 1 ;;
    esac
    # A supplied path with readelf unavailable is refused by name.
    set +e
    output=$(PATH=/nonexistent check_runner_libraries "$root/bin/true" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: missing readelf returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"readelf required to check RUNPATH of $root/bin/true"*) ;;
        *) echo "assert-test-dependencies --self-test: missing readelf was not named: $output" >&2; return 1 ;;
    esac
    # A non-ELF executable passes -x but readelf cannot read it: refused.
    set +e
    output=$(check_runner_libraries "$root/bin/true" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: non-ELF runner returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"cannot read dynamic section of runner binary: $root/bin/true"*) ;;
        *) echo "assert-test-dependencies --self-test: non-ELF runner was not named: $output" >&2; return 1 ;;
    esac
    # An empty argument is refused by name, never a pass.
    set +e
    output=$(check_runner_libraries "" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: empty runner path returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"runner binary path not supplied"*) ;;
        *) echo "assert-test-dependencies --self-test: empty runner path was not named: $output" >&2; return 1 ;;
    esac
    # The pointer fixture names exactly the runner the consumer would run,
    # and the production wiring checks that binary (here a non-ELF stub).
    mkdir -p "$root/bundle"
    printf '#!/bin/sh\nexit 0\n' >"$root/bundle/hermit"
    chmod +x "$root/bundle/hermit"
    printf '%s\n' "$root/bundle" >"$root/pointer.path"
    if [ "$(pointer_runner "$root/pointer.path")" != "$root/bundle/hermit" ]; then
        echo "assert-test-dependencies --self-test: pointer did not resolve to its bundle hermit" >&2
        return 1
    fi
    set +e
    output=$(check_pointer_runner "$root/pointer.path" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: pointer-named non-ELF runner returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"cannot read dynamic section of runner binary: $root/bundle/hermit"*) ;;
        *) echo "assert-test-dependencies --self-test: pointer-named runner was not named: $output" >&2; return 1 ;;
    esac
    set +e
    output=$(pointer_runner "$root/no-pointer.path" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: missing pointer returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"artifact pointer missing or empty: $root/no-pointer.path"*) ;;
        *) echo "assert-test-dependencies --self-test: missing pointer was not named: $output" >&2; return 1 ;;
    esac
    # Relative and two-line pointers fail the one-absolute-line shape check.
    printf 'relative/bundle\n' >"$root/relative.path"
    set +e
    output=$(pointer_runner "$root/relative.path" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: relative pointer returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"artifact pointer must contain one absolute path: $root/relative.path"*) ;;
        *) echo "assert-test-dependencies --self-test: relative pointer was not named: $output" >&2; return 1 ;;
    esac
    printf '%s\n%s\n' "$root/bundle" "$root/bundle" >"$root/two-line.path"
    set +e
    output=$(pointer_runner "$root/two-line.path" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: two-line pointer returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"artifact pointer must contain one absolute path: $root/two-line.path"*) ;;
        *) echo "assert-test-dependencies --self-test: two-line pointer was not named: $output" >&2; return 1 ;;
    esac
    # A directory used as the pointer is a named read refusal.
    set +e
    output=$(pointer_runner "$root/bundle" 2>&1)
    rc=$?
    set -e
    if [ "$rc" != 2 ]; then
        echo "assert-test-dependencies --self-test: directory pointer returned $rc, expected 2: $output" >&2
        return 1
    fi
    case "$output" in
        *"cannot read artifact pointer: $root/bundle"*) ;;
        *) echo "assert-test-dependencies --self-test: directory pointer was not named: $output" >&2; return 1 ;;
    esac
    echo "assert-test-dependencies --self-test: OK"
}

if [ "${1:-}" = --self-test ]; then
    self_test
    exit 0
fi

check_tools
check_guest_paths ""
# Always check the runner the artifact pointer names: the binary the
# consumer will run. A missing pointer or runner is a named refusal here,
# never a search for some other staged artifact.
check_pointer_runner
printf 'assert-test-dependencies: OK -- %d test tools, %d guest paths and runner libraries present\n' \
    "${#test_dependencies[@]}" "${#family_guest_paths[@]}"
