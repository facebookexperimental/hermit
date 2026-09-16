#!/usr/bin/env bash
# Assert the executable build dependencies carried by the pinned validate image.
# This is deliberately separate from run-split-validate.sh's guest-population
# audit: a command can be required by both, but xxd is required only while
# staging e9patch and must not be presented as a guest command.
# This shared container preflight also checks absolute guest paths separately;
# a present compiler does not prove that an integration guest can be executed.

set -euo pipefail

build_dependencies=(
    ar
    as
    c++
    cargo
    cat
    cc
    cmake
    cp
    g++
    gcc
    git
    ld
    make
    pkg-config
    ranlib
    rustc
    strip
    xxd
)

native_libraries=(libunwind elfutils zlib openssl)

check_build_dependencies() {
    local search_path=${1:-$PATH}
    local missing=()
    local tool

    for tool in "${build_dependencies[@]}"; do
        if ! PATH="$search_path" command -v "$tool" >/dev/null 2>&1; then
            missing+=("$tool")
        fi
    done

    if ((${#missing[@]} > 0)); then
        for tool in "${missing[@]}"; do
            echo "assert-build-dependencies: pinned root is missing required build dependency: $tool" >&2
        done
        echo "assert-build-dependencies: REFUSED -- ${#missing[@]} of ${#build_dependencies[@]} executable build dependencies are missing." >&2
        return 2
    fi

    echo "assert-build-dependencies: OK -- ${#build_dependencies[@]}/${#build_dependencies[@]} executable build dependencies are present." >&2
}

path_has_file() {
    local path_value=$1
    local relative=$2
    local directory
    local directories=()

    IFS=: read -r -a directories <<<"$path_value"
    for directory in "${directories[@]}"; do
        if [[ -n $directory && -f $directory/$relative ]]; then
            return 0
        fi
    done
    return 1
}

check_native_libraries() {
    local include_path=${1:-${CPATH:-}}
    local library_path=${2:-${LIBRARY_PATH:-}}
    local missing=()
    local library

    if ! path_has_file "$include_path" libunwind.h \
        || ! path_has_file "$library_path" libunwind.so \
        || ! path_has_file "$library_path" libunwind-ptrace.so; then
        missing+=(libunwind)
    fi
    if ! path_has_file "$include_path" elfutils/libdw.h \
        || ! path_has_file "$library_path" libdw.so \
        || ! path_has_file "$library_path" libelf.so; then
        missing+=(elfutils)
    fi
    if ! path_has_file "$include_path" zlib.h \
        || ! path_has_file "$library_path" libz.so; then
        missing+=(zlib)
    fi
    if ! path_has_file "$include_path" openssl/ssl.h \
        || ! path_has_file "$library_path" libssl.so \
        || ! path_has_file "$library_path" libcrypto.so; then
        missing+=(openssl)
    fi

    if ((${#missing[@]} > 0)); then
        for library in "${missing[@]}"; do
            echo "assert-build-dependencies: pinned root is missing required native library: $library" >&2
        done
        echo "assert-build-dependencies: REFUSED -- ${#missing[@]} of ${#native_libraries[@]} native libraries are missing." >&2
        return 2
    fi

    echo "assert-build-dependencies: OK -- ${#native_libraries[@]}/${#native_libraries[@]} native libraries and development headers are present." >&2
}

check_guest_paths() {
    local root=${1:-}
    local manifest
    manifest=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/guest-paths.txt
    local path count=0
    local missing=()
    local -A seen=()
    [[ -f $manifest ]] || {
        echo "assert-build-dependencies: missing guest-path manifest: $manifest" >&2
        return 2
    }
    while IFS= read -r path || [[ -n $path ]]; do
        if [[ ! $path =~ ^/usr/bin/[A-Za-z0-9_-]+$ || -v seen[$path] ]]; then
            echo "assert-build-dependencies: invalid or duplicate guest path: $path" >&2
            return 2
        fi
        seen[$path]=1
        count=$((count + 1))
        if [[ ! -f $root$path || ! -x $root$path ]]; then
            missing+=("$path")
        fi
    done < "$manifest"
    ((count > 0)) || {
        echo "assert-build-dependencies: empty guest-path manifest" >&2
        return 2
    }
    if ((${#missing[@]} > 0)); then
        for path in "${missing[@]}"; do
            echo "assert-build-dependencies: pinned root is missing required guest path: $path" >&2
        done
        echo "assert-build-dependencies: REFUSED -- ${#missing[@]} of $count absolute guest paths are missing or not executable files." >&2
        return 2
    fi
    echo "assert-build-dependencies: OK -- $count/$count absolute guest paths are executable files." >&2
}

self_test() {
    local fixture stub output rc tool
    fixture=$(mktemp -d)
    trap 'rm -rf -- "$fixture"' RETURN
    stub=/bin/true
    [[ -x $stub ]] || {
        echo "assert-build-dependencies --self-test: missing test stub $stub" >&2
        return 1
    }

    for tool in "${build_dependencies[@]}"; do
        ln -s "$stub" "$fixture/$tool"
    done
    output=$(check_build_dependencies "$fixture" 2>&1) || {
        echo "assert-build-dependencies --self-test: complete fixture was rejected" >&2
        echo "$output" >&2
        return 1
    }
    [[ $output == *"18/18 executable build dependencies are present"* ]] || {
        echo "assert-build-dependencies --self-test: success count was not reported" >&2
        echo "$output" >&2
        return 1
    }

    rm "$fixture/xxd"
    set +e
    output=$(check_build_dependencies "$fixture" 2>&1)
    rc=$?
    set -e
    [[ $rc -eq 2 ]] || {
        echo "assert-build-dependencies --self-test: missing xxd returned $rc, expected 2" >&2
        echo "$output" >&2
        return 1
    }
    [[ $output == *"missing required build dependency: xxd"* ]] || {
        echo "assert-build-dependencies --self-test: missing xxd was not named" >&2
        echo "$output" >&2
        return 1
    }
    [[ $output == *"1 of 18 executable build dependencies are missing"* ]] || {
        echo "assert-build-dependencies --self-test: refusal count was not reported" >&2
        echo "$output" >&2
        return 1
    }

    mkdir -p "$fixture/include/elfutils" "$fixture/include/openssl" "$fixture/lib"
    touch "$fixture/include/libunwind.h" \
        "$fixture/include/elfutils/libdw.h" \
        "$fixture/include/zlib.h" \
        "$fixture/include/openssl/ssl.h" \
        "$fixture/lib/libunwind.so" \
        "$fixture/lib/libunwind-ptrace.so" \
        "$fixture/lib/libdw.so" \
        "$fixture/lib/libelf.so" \
        "$fixture/lib/libz.so" \
        "$fixture/lib/libssl.so" \
        "$fixture/lib/libcrypto.so"
    output=$(check_native_libraries "$fixture/include" "$fixture/lib" 2>&1) || {
        echo "assert-build-dependencies --self-test: complete native library fixture was rejected" >&2
        echo "$output" >&2
        return 1
    }
    [[ $output == *"4/4 native libraries and development headers are present"* ]] || {
        echo "assert-build-dependencies --self-test: native library success count was not reported" >&2
        echo "$output" >&2
        return 1
    }

    rm "$fixture/lib/libunwind-ptrace.so"
    set +e
    output=$(check_native_libraries "$fixture/include" "$fixture/lib" 2>&1)
    rc=$?
    set -e
    [[ $rc -eq 2 ]] || {
        echo "assert-build-dependencies --self-test: missing libunwind-ptrace returned $rc, expected 2" >&2
        echo "$output" >&2
        return 1
    }
    [[ $output == *"missing required native library: libunwind"* ]] || {
        echo "assert-build-dependencies --self-test: missing libunwind-ptrace was not reported as libunwind" >&2
        echo "$output" >&2
        return 1
    }

    echo "PASS: assert-build-dependencies accepts 18/18 executables and 4/4 native libraries, and names xxd and libunwind when required files are absent"

    local guest_root=$fixture/guest-root manifest path
    manifest=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/guest-paths.txt
    mkdir -p "$guest_root/usr/bin"
    while IFS= read -r path; do
        ln -s "$stub" "$guest_root$path"
    done < "$manifest"
    check_guest_paths "$guest_root"

    # A PATH-resolvable printf is insufficient: the test executes /usr/bin/printf.
    # Also reject a non-executable file, directory or broken symlink at that path.
    local kind
    for kind in absent non-executable directory broken-link; do
        rm "$guest_root/usr/bin/printf"
        case $kind in
            absent) ;;
            non-executable) printf 'fixture\n' > "$guest_root/usr/bin/printf" ;;
            directory) mkdir "$guest_root/usr/bin/printf" ;;
            broken-link) ln -s "$guest_root/absent" "$guest_root/usr/bin/printf" ;;
        esac
        set +e
        output=$(check_guest_paths "$guest_root" 2>&1)
        rc=$?
        set -e
        [[ $rc -eq 2 && $output == *"missing required guest path: /usr/bin/printf"* ]] || {
            echo "assert-build-dependencies --self-test: $kind guest path was not refused ($rc): $output" >&2
            return 1
        }
        if [[ $kind == directory ]]; then
            rmdir "$guest_root/usr/bin/printf"
        elif [[ $kind != absent ]]; then
            rm "$guest_root/usr/bin/printf"
        fi
        ln -s "$stub" "$guest_root/usr/bin/printf"
    done
    check_guest_paths "$guest_root"
    echo "PASS: absolute guest paths reject absent/non-executable/directory/broken-link printf and accept the restored executable"
}

case ${1:-} in
    "")
        check_build_dependencies
        check_native_libraries
        check_guest_paths
        ;;
    --print)
        printf '%s\n' "${build_dependencies[@]}"
        ;;
    --self-test) self_test ;;
    *)
        echo "usage: assert-build-dependencies.sh [--print|--self-test]" >&2
        exit 2
        ;;
esac
