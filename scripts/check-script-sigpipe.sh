#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Regression guard for shared SIGPIPE handling and the rust-script entrypoint
# contract. ci/prepare-rust-scripts.sh owns compilation in one named DAG node;
# this checker proves that every tracked entrypoint uses the freshness-preserving
# shebang and that the producer's published inventory is current.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

fixture="scripts/lib/tests/sigpipe_smoke.rs"
[[ -f $fixture ]] || { echo "check-script-sigpipe.sh: missing $fixture" >&2; exit 2; }
command -v rustc >/dev/null 2>&1 || { echo "check-script-sigpipe.sh: rustc is required" >&2; exit 2; }
command -v realpath >/dev/null 2>&1 || { echo "check-script-sigpipe.sh: realpath is required" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
bin="$tmp/sigpipe_smoke"

RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}" rustc --edition=2021 -O "$fixture" -o "$bin"

status=0
out="$(set -o pipefail; "$bin" 2>"$tmp/err" | head -n1)" || status=$?
if [[ $status -ne 0 ]]; then
    echo "check-script-sigpipe.sh: FAIL — 'sigpipe_smoke | head' exited $status (want 0)" >&2
    echo "  (a SIGPIPE from an early consumer must be a clean exit, not 141/panic)" >&2
    echo "--- stderr ---" >&2
    cat "$tmp/err" >&2 || true
    exit 1
fi
if grep -qiE 'panic|Broken pipe|backtrace' "$tmp/err"; then
    echo "check-script-sigpipe.sh: FAIL — producer emitted a panic/EPIPE error on stderr:" >&2
    cat "$tmp/err" >&2
    exit 1
fi
if [[ $out != "line 0" ]]; then
    echo "check-script-sigpipe.sh: FAIL — unexpected first line: '$out' (want 'line 0')" >&2
    exit 1
fi
echo "check-script-sigpipe.sh: OK — SIGPIPE from an early consumer exits cleanly (0)"

tracked="$(git ls-files -- '*.rs')" || {
    echo "check-script-sigpipe.sh: cannot enumerate tracked Rust sources" >&2
    exit 2
}
consumers=0
while IFS= read -r source; do
    [[ -n $source ]] || continue
    IFS= read -r first <"$source" || {
        echo "check-script-sigpipe.sh: cannot read $source" >&2
        exit 2
    }
    case "$first" in
        '#!/usr/bin/env -S rust-script --force')
            [[ $(grep -Ec '^[[:space:]]*mod[[:space:]]+rust_script_prelude[[:space:]]*;' "$source") == 1 &&
               $(awk '
                   $0 ~ /^[[:space:]]*fn[[:space:]]+main\(\)([[:space:]]*->[[:space:]]*[^{}]+)?[[:space:]]*\{[[:space:]]*$/ { mains++ }
                   previous ~ /^[[:space:]]*fn[[:space:]]+main\(\)([[:space:]]*->[[:space:]]*[^{}]+)?[[:space:]]*\{[[:space:]]*$/ &&
                       $0 ~ /^[[:space:]]*rust_script_prelude::init\(\);[[:space:]]*$/ { count++ }
                   { previous = $0 }
                   END { print (mains + 0) ":" (count + 0) }
               ' "$source") == "1:1" ]] || {
                echo "check-script-sigpipe.sh: $source must have one main and initialize rust_script_prelude as its first statement" >&2
                exit 1
            }
            path_line="$(grep -B1 -m1 -E '^[[:space:]]*mod[[:space:]]+rust_script_prelude[[:space:]]*;' "$source" | head -n1)"
            prelude_relative="$(printf '%s\n' "$path_line" | sed -n 's/^[[:space:]]*#\[path = "\([^"]*\)"\][[:space:]]*$/\1/p')"
            if [[ -z $prelude_relative ||
                  $(realpath -- "$(dirname -- "$source")/$prelude_relative") != "$ROOT_DIR/scripts/lib/rust_script_prelude.rs" ]]; then
                echo "check-script-sigpipe.sh: $source must bind rust_script_prelude to scripts/lib/rust_script_prelude.rs" >&2
                exit 1
            fi
            consumers=$((consumers + 1))
            ;;
        *rust-script*)
            echo "check-script-sigpipe.sh: $source can reuse stale code: $first" >&2
            echo "  Use: #!/usr/bin/env -S rust-script --force" >&2
            exit 1
            ;;
    esac
done <<<"$tracked"
((consumers > 0)) || {
    echo "check-script-sigpipe.sh: no tracked rust-script entrypoints found" >&2
    exit 2
}
echo "check-script-sigpipe.sh: OK — $consumers tracked rust-script entrypoint(s) force Cargo freshness"

if [[ ${HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED:-} == 1 ]]; then
    ./ci/prepare-rust-scripts.sh --check
else
    ./ci/prepare-rust-scripts.sh
fi
echo "check-script-sigpipe.sh: OK — producer manifest covers all $consumers rust-script entrypoint(s)"

external_tmp="$(mktemp -d /tmp/hermit-rust-script-external.XXXXXXXX)"
trap 'rm -rf "$tmp" "$external_tmp"' EXIT
fake_rust_script="$external_tmp/rust-script"
external_source="$external_tmp/external.rs"
printf '%s\n' '#!/usr/bin/env bash' 'printf "delegated:%s\\n" "$*"' >"$fake_rust_script"
printf '%s\n' 'fn main() {}' >"$external_source"
chmod +x "$fake_rust_script"
delegated=$(HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1 \
    HERMIT_REAL_RUST_SCRIPT="$fake_rust_script" \
    ./ci/rust-script-bin/rust-script --force "$external_source" -- fixture)
[[ $delegated == "delegated:--force $external_source -- fixture" ]] || {
    echo "check-script-sigpipe.sh: external operational-tool script was not delegated: $delegated" >&2
    exit 1
}

unlisted_source="$ROOT_DIR/target/ci/rust-script-unlisted-$$.rs"
trap 'rm -rf "$tmp" "$external_tmp"; rm -f "$unlisted_source"' EXIT
printf '%s\n' 'fn main() {}' >"$unlisted_source"
if HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1 \
    HERMIT_REAL_RUST_SCRIPT="$fake_rust_script" \
    ./ci/rust-script-bin/rust-script --force "$unlisted_source" >"$tmp/unlisted.out" 2>"$tmp/unlisted.err"; then
    echo "check-script-sigpipe.sh: unlisted repository script bypassed the producer manifest" >&2
    exit 1
fi
grep -q 'producer manifest has no unique entry' "$tmp/unlisted.err" || {
    echo "check-script-sigpipe.sh: unlisted repository script failed for the wrong reason" >&2
    cat "$tmp/unlisted.err" >&2
    exit 1
}
echo "check-script-sigpipe.sh: OK — external tooling delegates; unlisted repository scripts refuse"

./ci/rust-script-bin/test-ownership.sh
./ci/rust-script-bin/test-log-isolation.sh
