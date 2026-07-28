#!/usr/bin/env bash
# Reject developer-specific homes and hostnames in tracked build/run files.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT_DIR

is_build_or_run_file() {
    case "$1" in
        *.sh | *.bash | *.rs | *.py | *.toml | *.yml | *.yaml | *.config \
            | *.conf | *.mk | Makefile | Makefile.* | */Makefile | */Makefile.* \
            | .github/* | ci/*)
            return 0 ;;
        *) return 1 ;;
    esac
}

is_excluded() {
    case "/$1/" in
        */.git/* | */ignored/* | */experiments/* | */scratch/* | */target/* \
            | */third-party/* | */vendor/* | */scripts/check-portable-paths.sh/)
            return 0 ;;
        *) return 1 ;;
    esac
}

scan_file() {
    local path=$1
    awk '
        {
            probe = tolower($0)
            gsub(/\/(home|users)\/(user|test|example)([^[:alnum:]_.-]|$)/,
                 "/generic/", probe)
            if (probe ~ /\/(home|users)\/[[:alnum:]_.-]+([^[:alnum:]_.-]|$)/ ||
                probe ~ /(^|[^[:alnum:]_])newton([^[:alnum:]_]|$)/ ||
                probe ~ /devbig[[:alnum:]._-]*/) {
                print FNR ":" $0
                found = 1
            }
        }
        END { exit found ? 1 : 0 }
    ' "$path"
}

check_repository() {
    local found=0
    local hit_file
    local path
    hit_file=$(mktemp "$ROOT_DIR/target/portable-path-hit.XXXXXX")
    while IFS= read -r -d '' path; do
        is_excluded "$path" && continue
        [[ -f $ROOT_DIR/$path ]] || continue
        is_build_or_run_file "$path" || [[ -x $ROOT_DIR/$path ]] || continue
        if ! scan_file "$ROOT_DIR/$path" >"$hit_file"; then
            while IFS= read -r hit; do
                printf '%s:%s\n' "$path" "$hit"
            done <"$hit_file"
            found=1
        fi
    done < <(git -C "$ROOT_DIR" ls-files -z)
    rm -f "$hit_file"
    return "$found"
}

self_test() {
    local fixture
    fixture=$(mktemp)

    printf '%s\n' "cache_dir=\"\${HOME}/.cache/hermit\"" >"$fixture"
    scan_file "$fixture" >/dev/null || {
        echo "portability self-test rejected a HOME-relative path" >&2
        rm -f "$fixture"
        return 1
    }

    printf 'cache_dir="/home/ci-portability-owner/.cache/hermit"\n' >"$fixture"
    if scan_file "$fixture" >/dev/null; then
        echo "portability self-test failed to reject a literal developer home" >&2
        rm -f "$fixture"
        return 1
    fi
    rm -f "$fixture"
}

mkdir -p "$ROOT_DIR/target"
self_test
if ! check_repository; then
    echo "portability check failed: replace literal homes/hosts with HOME, repo-relative paths, PATH lookup, or an explicit environment override" >&2
    exit 1
fi

echo "Portability path check passed."
