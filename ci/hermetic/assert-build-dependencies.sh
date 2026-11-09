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
    objdump
    pkg-config
    ranlib
    readelf
    rustc
    strip
    xxd
)

# xz provides liblzma. libunwind's pkg-config metadata names -llzma in
# Libs.private and reverie-ptrace asks for that --static closure, so a link
# without liblzma.so fails late with `unable to find library -llzma`.
# abseil-cpp, googletest and benchmark carry the vendored gVisor syscall
# tests' C++ dependencies; on the host they resolve through pkg-config
# system libraries, so the pinned root must name them before the Buck build
# does. gbenchmark ships only the static archives (no libbenchmark.so), so
# its check names the archive the linker actually consumes.
native_libraries=(libunwind elfutils zlib openssl xz abseil-cpp googletest benchmark)

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
    if ! path_has_file "$include_path" lzma.h \
        || ! path_has_file "$library_path" liblzma.so; then
        missing+=(xz)
    fi
    if ! path_has_file "$include_path" absl/strings/string_view.h \
        || ! path_has_file "$library_path" libabsl_strings.so \
        || ! path_has_file "$library_path" libabsl_time.so; then
        missing+=(abseil-cpp)
    fi
    if ! path_has_file "$include_path" gtest/gtest.h \
        || ! path_has_file "$library_path" libgtest.so \
        || ! path_has_file "$library_path" libgmock.so; then
        missing+=(googletest)
    fi
    if ! path_has_file "$include_path" benchmark/benchmark.h \
        || ! path_has_file "$library_path" libbenchmark.a; then
        missing+=(benchmark)
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
    # The manifest is the exact allowlist: every literal path the corpus
    # names, one per line, reviewed with the image. There is deliberately
    # no name-pattern gate here — an earlier /usr/bin/[A-Za-z0-9_-] shape
    # (widened to a basename charset on 2026-10-30) refused legitimate
    # listed paths (/usr/bin/[, /usr/bin/c++filt, /usr/sbin/ip) while
    # admitting unlisted ones that happened to match the charset. A listed
    # line is admitted exactly as written; the only structural rules are
    # that it is a canonical absolute path (no empty, dot or dot-dot
    # components, no whitespace) and not a duplicate.
    local manifest=${2:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/guest-paths.txt}
    local path count=0
    local missing=()
    # Duplicate detection compares whole lines literally. An associative
    # array keyed by the path would mis-key names containing [ or ], which
    # the exact allowlist deliberately admits (/usr/bin/[).
    local seen_list=$'\n'
    [[ -f $manifest ]] || {
        echo "assert-build-dependencies: missing guest-path manifest: $manifest" >&2
        return 2
    }
    while IFS= read -r path || [[ -n $path ]]; do
        if [[ $path != /* || $path == *[[:space:]]* || $path == *//* \
            || $path == */. || $path == */.. || $path == */./* || $path == */../* \
            || $seen_list == *$'\n'"$path"$'\n'* ]]; then
            echo "assert-build-dependencies: invalid or duplicate guest path: $path" >&2
            return 2
        fi
        seen_list+="$path"$'\n'
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
    [[ $output == *"20/20 executable build dependencies are present"* ]] || {
        echo "assert-build-dependencies --self-test: success count was not reported" >&2
        echo "$output" >&2
        return 1
    }

    for tool in xxd readelf objdump; do
        rm "$fixture/$tool"
        set +e
        output=$(check_build_dependencies "$fixture" 2>&1)
        rc=$?
        set -e
        [[ $rc -eq 2 ]] || {
            echo "assert-build-dependencies --self-test: missing $tool returned $rc, expected 2" >&2
            echo "$output" >&2
            return 1
        }
        [[ $output == *"missing required build dependency: $tool"* ]] || {
            echo "assert-build-dependencies --self-test: missing $tool was not named" >&2
            echo "$output" >&2
            return 1
        }
        [[ $output == *"1 of 20 executable build dependencies are missing"* ]] || {
            echo "assert-build-dependencies --self-test: refusal count was not reported" >&2
            echo "$output" >&2
            return 1
        }
        ln -s "$stub" "$fixture/$tool"
    done

    mkdir -p "$fixture/include/elfutils" "$fixture/include/openssl" \
        "$fixture/include/absl/strings" "$fixture/include/gtest" \
        "$fixture/include/benchmark" "$fixture/lib"
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
        "$fixture/lib/libcrypto.so" \
        "$fixture/include/lzma.h" \
        "$fixture/lib/liblzma.so" \
        "$fixture/include/absl/strings/string_view.h" \
        "$fixture/lib/libabsl_strings.so" \
        "$fixture/lib/libabsl_time.so" \
        "$fixture/include/gtest/gtest.h" \
        "$fixture/lib/libgtest.so" \
        "$fixture/lib/libgmock.so" \
        "$fixture/include/benchmark/benchmark.h" \
        "$fixture/lib/libbenchmark.a"
    output=$(check_native_libraries "$fixture/include" "$fixture/lib" 2>&1) || {
        echo "assert-build-dependencies --self-test: complete native library fixture was rejected" >&2
        echo "$output" >&2
        return 1
    }
    [[ $output == *"8/8 native libraries and development headers are present"* ]] || {
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
    touch "$fixture/lib/libunwind-ptrace.so"

    # The runtime liblzma.so.5 alone does not satisfy -llzma; the link needs
    # the unversioned development name.
    rm "$fixture/lib/liblzma.so"
    touch "$fixture/lib/liblzma.so.5"
    set +e
    output=$(check_native_libraries "$fixture/include" "$fixture/lib" 2>&1)
    rc=$?
    set -e
    [[ $rc -eq 2 && $output == *"missing required native library: xz"* \
        && $output == *"1 of 8 native libraries are missing"* ]] || {
        echo "assert-build-dependencies --self-test: missing liblzma.so was not refused as xz ($rc): $output" >&2
        return 1
    }
    touch "$fixture/lib/liblzma.so"

    rm "$fixture/lib/libabsl_time.so"
    set +e
    output=$(check_native_libraries "$fixture/include" "$fixture/lib" 2>&1)
    rc=$?
    set -e
    [[ $rc -eq 2 && $output == *"missing required native library: abseil-cpp"* ]] || {
        echo "assert-build-dependencies --self-test: missing libabsl_time.so was not refused as abseil-cpp ($rc): $output" >&2
        return 1
    }
    touch "$fixture/lib/libabsl_time.so"

    echo "PASS: assert-build-dependencies accepts 20/20 executables and 8/8 native libraries, and names xxd, readelf, objdump, libunwind, xz and abseil-cpp when required files are absent"

    local guest_root=$fixture/guest-root manifest path
    manifest=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/guest-paths.txt
    mkdir -p "$guest_root/usr/bin"
    while IFS= read -r path; do
        mkdir -p "$guest_root$(dirname -- "$path")"
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

    # The manifest is the exact allowlist: literal paths with characters
    # outside any identifier charset are admitted exactly as listed, and
    # non-canonical or duplicate lines are refused. (The name-pattern gate
    # this replaces refused /usr/bin/[ and /usr/sbin/ip, both listed.)
    local exact_root=$fixture/exact-root exact_manifest=$fixture/exact-paths.txt bad
    mkdir -p "$exact_root/usr/bin" "$exact_root/usr/sbin"
    printf '%s\n' /usr/bin/[ /usr/bin/c++filt /usr/sbin/ip > "$exact_manifest"
    ln -s "$stub" "$exact_root/usr/bin/["
    ln -s "$stub" "$exact_root/usr/bin/c++filt"
    ln -s "$stub" "$exact_root/usr/sbin/ip"
    check_guest_paths "$exact_root" "$exact_manifest"
    for bad in 'usr/bin/[' '/usr/bin/../etc/passwd' '/usr/bin//[' '/usr/bin/['$'\n''/usr/bin/['; do
        printf '%s\n' "$bad" > "$exact_manifest"
        set +e
        output=$(check_guest_paths "$exact_root" "$exact_manifest" 2>&1)
        rc=$?
        set -e
        [[ $rc -eq 2 ]] || {
            echo "assert-build-dependencies --self-test: non-canonical or duplicate guest path was not refused ($rc): $bad" >&2
            echo "$output" >&2
            return 1
        }
    done
    echo "PASS: literal guest paths ([, c++filt, sbin) accepted exactly as listed; relative/dot-dot/empty-component/duplicate lines refused"
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
