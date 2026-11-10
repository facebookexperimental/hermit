#!/usr/bin/env bash
# Check that every oss_cargo/<crate>/Cargo.toml.oss is a byte-identical twin of
# <crate>/Cargo.toml.
#
# Meta's internal tree keeps the public manifest of the crates below at
# oss_cargo/<crate>/Cargo.toml.oss, and the export to facebookexperimental/hermit
# carries those files, so each ancestry merge brings them here. Cargo reads only
# the copy at the crate path. An edit to a crate's Cargo.toml that misses its
# .oss twin therefore goes unnoticed here and reaches the internal tree as a
# stale manifest on the next import. Every pair is equal from the 0.5.6 ancestry
# merge on; this check keeps them equal.
#
# It fails when:
#   - the oss_cargo/**/Cargo.toml.oss files are not exactly the twins of CRATES
#     below (a twin is missing, or a new one is undeclared);
#   - a twin or its crate manifest is missing, a symbolic link, or not a regular
#     file;
#   - a pair differs in bytes or in the executable bit.
#
# Moving these manifests to their crate paths in the internal tree retires
# oss_cargo/, and this check goes with it.
#
# Usage: scripts/check-oss-cargo-twins.sh [ROOT] | --self-test
#   ROOT defaults to the checkout that holds this script.

set -euo pipefail

SCRIPT_PATH=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/$(basename -- "${BASH_SOURCE[0]}")
readonly SCRIPT_PATH

# The crates whose public manifest also lives under oss_cargo/.
readonly CRATES=(
    detcore-liteinst
    detcore/tests
    detcore/tests/lit-guests
    hermit-cdylib
    hermit-record
    hermit-record-format
    hermit-replay
    hermit-run-kvm
    hermit-run-ptrace
    inguest-engine
)

# check ROOT: print one line per problem and return 1 if there is any.
check() {
    local root=$1 crate twin manifest file problems=0 twin_x manifest_x
    local -A declared=()
    local -a found=()
    for crate in "${CRATES[@]}"; do
        declared[$crate]=1
    done
    if [ -d "$root/oss_cargo" ]; then
        while IFS= read -r -d '' twin; do
            twin=${twin#"$root/oss_cargo/"}
            found+=("${twin%/Cargo.toml.oss}")
        done < <(find "$root/oss_cargo" -name Cargo.toml.oss -print0)
    fi
    for crate in "${found[@]}"; do
        if [ -z "${declared[$crate]+set}" ]; then
            echo "oss_cargo/$crate/Cargo.toml.oss: not a declared twin; add $crate to CRATES in scripts/check-oss-cargo-twins.sh, or remove the file"
            problems=$((problems + 1))
        fi
    done
    for crate in "${CRATES[@]}"; do
        twin=oss_cargo/$crate/Cargo.toml.oss
        manifest=$crate/Cargo.toml
        for file in "$twin" "$manifest"; do
            if [ -L "$root/$file" ] || [ ! -f "$root/$file" ]; then
                echo "$file: missing, or not a regular file"
                problems=$((problems + 1))
                continue 2
            fi
        done
        if ! cmp -s -- "$root/$twin" "$root/$manifest"; then
            echo "$twin differs from $manifest; give both the same bytes"
            problems=$((problems + 1))
        fi
        twin_x=no
        manifest_x=no
        if [ -x "$root/$twin" ]; then twin_x=yes; fi
        if [ -x "$root/$manifest" ]; then manifest_x=yes; fi
        if [ "$twin_x" != "$manifest_x" ]; then
            echo "$twin (executable: $twin_x) and $manifest (executable: $manifest_x) differ in mode"
            problems=$((problems + 1))
        fi
    done
    if [ "$problems" -gt 0 ]; then
        echo "oss-cargo-twins: $problems problem(s)"
        return 1
    fi
    echo "oss-cargo-twins: OK, all ${#CRATES[@]} oss_cargo twins equal their crate manifests"
}

self_test() {
    local tmp tree crate failures=0
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/oss-cargo-twins-test.XXXXXX")
    trap 'rm -rf "$tmp"' RETURN
    tree=$tmp/tree
    # A tree in which every twin equals its manifest.
    fresh_tree() {
        rm -rf "$tree"
        for crate in "${CRATES[@]}"; do
            mkdir -p "$tree/$crate" "$tree/oss_cargo/$crate"
            printf '[package]\nname = "%s"\nversion = "0.1.0"\n' "${crate##*/}" > "$tree/$crate/Cargo.toml"
            cp "$tree/$crate/Cargo.toml" "$tree/oss_cargo/$crate/Cargo.toml.oss"
        done
    }
    expect() { # name want-exit
        local rc=0
        bash "$SCRIPT_PATH" "$tree" > /dev/null 2>&1 || rc=$?
        if [ "$rc" = "$2" ]; then
            echo "self-test PASS: $1"
        else
            echo "self-test FAIL: $1 (want exit $2, got $rc)" >&2
            failures=$((failures + 1))
        fi
    }
    fresh_tree
    expect "equal twins pass" 0
    sed -i 's/0\.1\.0/0.1.1/' "$tree/oss_cargo/${CRATES[0]}/Cargo.toml.oss"
    expect "a one-byte edit to a twin fails" 1
    fresh_tree
    printf '\n' >> "$tree/${CRATES[1]}/Cargo.toml"
    expect "an edit to a crate manifest alone fails" 1
    fresh_tree
    chmod +x "$tree/oss_cargo/${CRATES[2]}/Cargo.toml.oss"
    expect "a mode change fails" 1
    fresh_tree
    rm "$tree/oss_cargo/${CRATES[3]}/Cargo.toml.oss"
    expect "a missing twin fails" 1
    fresh_tree
    mkdir -p "$tree/oss_cargo/undeclared-crate"
    cp "$tree/${CRATES[0]}/Cargo.toml" "$tree/oss_cargo/undeclared-crate/Cargo.toml.oss"
    expect "an undeclared twin fails" 1
    fresh_tree
    ln -sf "$tree/${CRATES[4]}/Cargo.toml" "$tree/oss_cargo/${CRATES[4]}/Cargo.toml.oss"
    expect "a twin that is a symbolic link fails" 1
    if [ "$failures" -eq 0 ]; then
        echo "oss-cargo-twins self-test: all cases passed"
    else
        echo "oss-cargo-twins self-test: $failures failure(s)" >&2
        return 1
    fi
}

case "${1:-}" in
    --self-test)
        self_test
        ;;
    -h | --help)
        sed -n '2,24p' "$SCRIPT_PATH"
        ;;
    "")
        check "$(dirname -- "$(dirname -- "$SCRIPT_PATH")")"
        ;;
    *)
        check "$1"
        ;;
esac
