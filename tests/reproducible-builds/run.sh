#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)
fixture="$script_dir/build-time-0.1.3"
hermit_bin=${HERMIT_BIN:-"$repo_root/target/release/hermit"}
artifact_dir="$fixture/target/reproducible-builds"

if [[ ! -x "$hermit_bin" ]]; then
    printf 'Hermit binary not found: %s\n' "$hermit_bin" >&2
    printf 'Build it with: cargo build --release -p hermit --bin hermit\n' >&2
    exit 1
fi

if [[ $(rustc --version) != *nightly* ]]; then
    printf "The fixture requires Hermit's pinned nightly rustc.\n" >&2
    exit 1
fi

# Build the immutable proc-macro dependency once. The measured leaf crate is
# compiled separately below so each run expands build_time_utc! afresh.
cargo build --manifest-path "$fixture/Cargo.toml" --release --locked
macro=$(find "$fixture/target/release/deps" -maxdepth 1 \
    -name 'libbuild_time-*.so' -print -quit)
if [[ -z "$macro" ]]; then
    printf 'Could not locate the build-time proc-macro artifact.\n' >&2
    exit 1
fi

mkdir -p "$artifact_dir"
compile=(
    rustc
    -Z threads=1
    --edition=2021
    --crate-name hermit_repro_build_time
    --crate-type lib
    --emit=obj
    -C opt-level=3
    -L "dependency=$fixture/target/release/deps"
    --extern "build_time=$macro"
    "$fixture/src/lib.rs"
)

"${compile[@]}" -o "$artifact_dir/native-one.o"
sleep 1
"${compile[@]}" -o "$artifact_dir/native-two.o"

native_one=$(sha256sum "$artifact_dir/native-one.o" | cut -d ' ' -f1)
native_two=$(sha256sum "$artifact_dir/native-two.o" | cut -d ' ' -f1)
if cmp -s "$artifact_dir/native-one.o" "$artifact_dir/native-two.o"; then
    printf 'Native builds unexpectedly matched: %s\n' "$native_one" >&2
    exit 1
fi

"$hermit_bin" run --strict --workdir "$fixture" -- \
    "${compile[@]}" -o "$artifact_dir/hermit-one.o"
"$hermit_bin" run --strict --workdir "$fixture" -- \
    "${compile[@]}" -o "$artifact_dir/hermit-two.o"

hermit_one=$(sha256sum "$artifact_dir/hermit-one.o" | cut -d ' ' -f1)
hermit_two=$(sha256sum "$artifact_dir/hermit-two.o" | cut -d ' ' -f1)
if ! cmp -s "$artifact_dir/hermit-one.o" "$artifact_dir/hermit-two.o"; then
    printf 'Hermit builds differed: %s != %s\n' "$hermit_one" "$hermit_two" >&2
    exit 1
fi

# Pre-create the output so both verification runs observe the same unlink.
: > "$artifact_dir/verified.o"
# The single-quoted variables expand in the guest bash process.
# shellcheck disable=SC2016
"$hermit_bin" run --strict --verify --workdir "$fixture" -- \
    bash -c 'output=$1; shift; rm -f "$output"; "$@" -o "$output"' \
    _ "$artifact_dir/verified.o" "${compile[@]}"

printf 'native-one  %s\n' "$native_one"
printf 'native-two  %s\n' "$native_two"
printf 'hermit-one  %s\n' "$hermit_one"
printf 'hermit-two  %s\n' "$hermit_two"
printf 'PASS: native artifacts differ; strict ptrace Hermit artifacts match.\n'
