#!/usr/bin/env bash
# Build every tracked rust-script entrypoint and its test harness before consumers run.

set -euo pipefail

case ${1:-} in
    '') mode=build ;;
    --check) mode=check ;;
    --fetch-only) mode=fetch ;;
    *)
        printf 'usage: %s [--check|--fetch-only]\n' "$0" >&2
        exit 2
        ;;
esac

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

find_real_rust_script() {
    local candidate dir
    if [[ -n ${HERMIT_REAL_RUST_SCRIPT:-} ]]; then
        candidate=$HERMIT_REAL_RUST_SCRIPT
        [[ -x $candidate ]] || {
            printf 'prepare-rust-scripts: HERMIT_REAL_RUST_SCRIPT is not executable: %s\n' "$candidate" >&2
            return 1
        }
        printf '%s\n' "$candidate"
        return 0
    fi
    IFS=: read -ra path_entries <<<"${PATH:-}"
    for dir in "${path_entries[@]}"; do
        [[ -n $dir ]] || dir=.
        candidate=$dir/rust-script
        [[ -x $candidate ]] || continue
        if [[ $(realpath -- "$candidate") != "$ROOT_DIR/ci/rust-script-bin/rust-script" ]]; then
            realpath -- "$candidate"
            return 0
        fi
    done
    printf 'prepare-rust-scripts: real rust-script executable not found outside ci/rust-script-bin\n' >&2
    return 1
}

real_rust_script=$(find_real_rust_script) || exit 2
command -v flock >/dev/null 2>&1 || {
    echo 'prepare-rust-scripts: flock is required' >&2
    exit 2
}

submodule_diagnosis() {
    local manifest=$1
    local unpopulated='' wrong_revision='' path recorded actual
    while IFS= read -r path; do
        [[ -n $path ]] || continue
        grep -q "/$path/" "$manifest" 2>/dev/null || continue
        recorded=$(git ls-tree HEAD "$path" 2>/dev/null | awk '{print $3}')
        [[ -n $recorded ]] || continue
        if [[ -z $(ls -A "$path" 2>/dev/null) ]]; then
            unpopulated+=" $path"
            continue
        fi
        actual=$(git -C "$path" rev-parse HEAD 2>/dev/null || true)
        if [[ -n $actual && $actual != "$recorded" ]]; then
            wrong_revision+=" $path"
        fi
    done < <(git ls-files --stage | awk '$1 == "160000" {print $4}')
    if [[ -n $unpopulated ]]; then
        printf 'unpopulated:%s' "$unpopulated"
    elif [[ -n $wrong_revision ]]; then
        printf 'wrongrev:%s' "$wrong_revision"
    else
        printf 'clean'
    fi
}

report_cargo_failure() {
    local source=$1 manifest=$2 output=$3 action=$4 diagnosis submodule
    diagnosis=$(submodule_diagnosis "$manifest")
    case $diagnosis in
        unpopulated:*)
            printf 'prepare-rust-scripts: REFUSED — cannot %s %s: required submodule(s) are unpopulated:%s\n' \
                "$action" "$source" "${diagnosis#unpopulated:}" >&2
            printf '  Run: git submodule update --init%s\n' "${diagnosis#unpopulated:}" >&2
            cat "$output" >&2
            return 2
            ;;
        wrongrev:*)
            printf 'prepare-rust-scripts: REFUSED — cannot %s %s: required submodule(s) are at the wrong revision:%s\n' \
                "$action" "$source" "${diagnosis#wrongrev:}" >&2
            for submodule in ${diagnosis#wrongrev:}; do
                printf '  %s recorded=%s checked-out=%s\n' "$submodule" \
                    "$(git ls-tree HEAD "$submodule" | awk '{print $3}')" \
                    "$(git -C "$submodule" rev-parse HEAD 2>/dev/null || true)" >&2
            done
            printf '  Run: git submodule update --init%s\n' "${diagnosis#wrongrev:}" >&2
            cat "$output" >&2
            return 2
            ;;
    esac
    if grep -qE 'unable to update /|failed to read .*/Cargo\.toml' "$output"; then
        printf 'prepare-rust-scripts: REFUSED — cannot %s %s because a path dependency could not be resolved\n' \
            "$action" "$source" >&2
        cat "$output" >&2
        return 2
    fi
    if grep -qE 'Could not resolve|failed to download|download of config\.json failed|network failure' "$output"; then
        printf 'prepare-rust-scripts: REFUSED — cannot %s %s because Cargo could not reach a required registry or repository\n' \
            "$action" "$source" >&2
        cat "$output" >&2
        return 2
    fi
    printf 'prepare-rust-scripts: FAIL — cannot %s %s\n' "$action" "$source" >&2
    cat "$output" >&2
    return 1
}

parent=$ROOT_DIR/target/ci
published=$parent/rust-scripts
build_target=$parent/rust-script-build
mkdir -p "$parent"
if [[ $mode == fetch ]]; then
    # Fetching writes only its private generated workspace and CARGO_HOME. Do
    # not serialize it behind the ordinary producer, which writes the
    # published binaries and can take minutes in a clean checkout.
    exec 8>"$parent/rust-scripts-fetch.lock"
    flock 8
else
    exec 9>"$parent/rust-scripts.lock"
    flock 9
fi

tracked=$(git ls-files -- '*.rs') || {
    echo 'prepare-rust-scripts: cannot enumerate tracked Rust sources' >&2
    exit 2
}
worktree_status=$(git status --porcelain=v1 --untracked-files=all --ignore-submodules=none) || {
    echo 'prepare-rust-scripts: cannot inspect the working tree' >&2
    exit 2
}
tree_clean=1
[[ -z $worktree_status ]] || tree_clean=0
entrypoints=()
test_entrypoints=()
while IFS= read -r source; do
    [[ -n $source ]] || continue
    IFS= read -r first <"$source" || {
        printf 'prepare-rust-scripts: cannot read %s\n' "$source" >&2
        exit 2
    }
    if [[ $first == '#!/usr/bin/env -S rust-script --force' ]]; then
        entrypoints+=("$source")
        if grep -q '#\[cfg(test)\]' -- "$source"; then
            test_entrypoints+=("$source")
        fi
    fi
done <<<"$tracked"
((${#entrypoints[@]} > 0)) || {
    echo 'prepare-rust-scripts: no tracked rust-script entrypoints found' >&2
    exit 2
}

state_input=$(mktemp)
state_after=$(mktemp)
scratch=$(mktemp -d "$parent/.rust-scripts.XXXXXXXX")
packages=$(mktemp -d "${TMPDIR:-/tmp}/hermit-rust-script-packages.XXXXXXXX")
cleanup() {
    local rc=$?
    rm -f -- "$state_input" "$state_after"
    rm -rf -- "$scratch" "$packages"
    exit "$rc"
}
trap cleanup EXIT

write_state() {
    printf 'schema=1\nhead=%s\n' "$(git rev-parse HEAD)"
    printf 'rust-script=%s\n' "$($real_rust_script --version)"
    rustc -Vv
    printf 'RUSTFLAGS=%s\nCARGO_ENCODED_RUSTFLAGS=%s\nRUSTUP_TOOLCHAIN=%s\nCARGO_BUILD_TARGET=%s\n' \
        "${RUSTFLAGS:-}" "${CARGO_ENCODED_RUSTFLAGS:-}" "${RUSTUP_TOOLCHAIN:-}" \
        "${CARGO_BUILD_TARGET:-}"
    git diff --binary HEAD
    while IFS= read -r -d '' source; do
        printf 'untracked=%s\n' "$source"
        sha256sum -- "$source"
    done < <(git ls-files --others --exclude-standard -z | sort -z)
}
write_state >"$state_input"
state=$(sha256sum "$state_input" | awk '{print $1}')

manifest_is_complete() {
    ((tree_clean)) || return 1
    [[ -f $published/stamp && -f $published/manifest.tsv ]] || return 1
    [[ $(<"$published/stamp") == "$state" ]] || return 1
    [[ $(wc -l <"$published/manifest.tsv") -eq ${#entrypoints[@]} ]] || return 1
    local source run_path test_path expected_test matches
    while IFS=$'\t' read -r source run_path test_path; do
        [[ -n $source && -x $published/$run_path ]] || return 1
        [[ $test_path == - || -x $published/$test_path ]] || return 1
    done <"$published/manifest.tsv"
    for source in "${entrypoints[@]}"; do
        matches=$(awk -F '\t' -v source="$source" '$1 == source { count++ } END { print count + 0 }' \
            "$published/manifest.tsv")
        [[ $matches == 1 ]] || return 1
        expected_test=-
        if grep -q '#\[cfg(test)\]' -- "$source"; then
            expected_test=present
        fi
        if [[ $expected_test == present ]]; then
            awk -F '\t' -v source="$source" '$1 == source && $3 != "-" { found++ } END { exit(found == 1 ? 0 : 1) }' \
                "$published/manifest.tsv" || return 1
        else
            awk -F '\t' -v source="$source" '$1 == source && $3 == "-" { found++ } END { exit(found == 1 ? 0 : 1) }' \
                "$published/manifest.tsv" || return 1
        fi
    done
}

if [[ $mode != fetch ]] && manifest_is_complete; then
    printf 'prepare-rust-scripts: reused %d entrypoints (%d test harnesses) from %s\n' \
        "${#entrypoints[@]}" "${#test_entrypoints[@]}" "$published"
    exit 0
fi
if [[ $mode == check ]]; then
    printf 'prepare-rust-scripts: prepared binaries are absent, stale, or incomplete; run %s\n' \
        "$ROOT_DIR/ci/prepare-rust-scripts.sh" >&2
    exit 2
fi

command -v cargo >/dev/null 2>&1 || {
    echo 'prepare-rust-scripts: cargo is required' >&2
    exit 2
}
command -v jq >/dev/null 2>&1 || {
    echo 'prepare-rust-scripts: jq is required' >&2
    exit 2
}

mkdir -p "$scratch/run" "$scratch/test" "$build_target"
: >"$scratch/manifest.tsv"

CLIPPY_WAIVERS=(
    -A clippy::doc_overindented_list_items
    -A clippy::doc_lazy_continuation
    -A clippy::empty_line_after_doc_comments
    -A clippy::too_many_arguments
    -A clippy::type_complexity
)

keys=()
for source in "${entrypoints[@]}"; do
    key=$(printf '%s' "$source" | sha256sum | cut -c1-16)
    keys+=("$key")
    package_dir=$packages/$key
    output=$packages/$key.output
    if ! "$real_rust_script" --package --pkg-path "$package_dir" "$source" >"$output" 2>&1; then
        printf 'prepare-rust-scripts: cannot generate Cargo package for %s\n' "$source" >&2
        cat "$output" >&2
        exit 2
    fi
done

# Resolve the generated packages together. Each script remains a separate Cargo
# package with its own dependency declaration, while one workspace lockfile
# stops Cargo from resolving the same registry and git graph once per script.
# Keep the single outer flock above: this is one writer using Cargo's internal
# parallelism, not competing writers racing the published directory.
workspace_manifest=$packages/Cargo.toml
{
    printf '[workspace]\nresolver = "2"\nmembers = [\n'
    printf '  "%s",\n' "${keys[@]}"
    printf ']\n\n[profile.release]\nstrip = true\n'
} >"$workspace_manifest"
metadata=$packages/metadata.json
output=$packages/workspace.output
if ! cargo metadata --format-version 1 --no-deps --manifest-path "$workspace_manifest" \
    >"$metadata" 2>"$output"; then
    printf 'prepare-rust-scripts: cannot resolve the generated workspace\n' >&2
    cat "$output" >&2
    exit 2
fi
package_count=$(jq -er '.packages | length' "$metadata") || exit 2
if ((package_count != ${#entrypoints[@]})); then
    printf 'prepare-rust-scripts: generated workspace has %d packages, expected %d\n' \
        "$package_count" "${#entrypoints[@]}" >&2
    exit 2
fi

if [[ $mode == fetch ]]; then
    # These packages are generated, so they have no committed lockfile. Resolve
    # one while the network is available, then make the download itself obey
    # that exact lock. The later pinned-root producer runs offline against this
    # CARGO_HOME; it must never depend on an unrelated warm host cache.
    output=$packages/workspace.output
    if ! cargo generate-lockfile --manifest-path "$workspace_manifest" >"$output" 2>&1; then
        report_cargo_failure 'the generated rust-script workspace' "$workspace_manifest" "$output" \
            'resolve dependencies for' || exit $?
    fi
    cat "$output"
    if ! cargo fetch --locked --manifest-path "$workspace_manifest" >"$output" 2>&1; then
        report_cargo_failure 'the generated rust-script workspace' "$workspace_manifest" "$output" \
            'fetch dependencies for' || exit $?
    fi
    cat "$output"
    printf 'prepare-rust-scripts: fetched dependencies for %d entrypoints into %s\n' \
        "${#entrypoints[@]}" "${CARGO_HOME:-the active Cargo home}"
    exit 0
fi

if ! cargo clippy -V >"$packages/clippy-version.out" 2>&1; then
    echo 'prepare-rust-scripts: REFUSED — cargo clippy is unavailable, so no script has been checked' >&2
    echo '  Install it with: rustup component add clippy' >&2
    cat "$packages/clippy-version.out" >&2
    exit 2
fi

command -v strip >/dev/null 2>&1 || {
    echo 'prepare-rust-scripts: strip is required to publish bounded artifacts' >&2
    exit 2
}

package_names=()
test_package_args=()
for index in "${!entrypoints[@]}"; do
    source=${entrypoints[$index]}
    key=${keys[$index]}
    package_dir=$packages/$key
    package_manifest=$(realpath -- "$package_dir/Cargo.toml")
    if ! package_name=$(jq -er --arg manifest "$package_manifest" '
        [.packages[] | select(.manifest_path == $manifest)
         | .targets[] | select(.kind == ["bin"]) | .name]
        | if length == 1 then .[0] else empty end
    ' "$metadata"); then
        printf 'prepare-rust-scripts: cannot identify generated binary target for %s\n' "$source" >&2
        exit 2
    fi
    package_names+=("$package_name")
    if grep -q '#\[cfg(test)\]' -- "$source"; then
        test_package_args+=(--package "$package_name")
    fi
done

# One Cargo invocation per phase keeps its internal dependency graph intact, so
# common dependencies compile once and Cargo chooses the safe parallel width.
# The outer flock still makes this the only writer to the persistent target and
# published directories.
output=$packages/workspace.output
if ! cargo clippy --manifest-path "$workspace_manifest" --workspace \
    --target-dir "$build_target" \
    -- -D warnings "${CLIPPY_WAIVERS[@]}" >"$output" 2>&1; then
    report_cargo_failure 'the generated rust-script workspace' "$workspace_manifest" "$output" \
        'check with clippy' || exit $?
fi
cat "$output"
if ! cargo build --release --manifest-path "$workspace_manifest" --workspace \
    --target-dir "$build_target" >"$output" 2>&1; then
    report_cargo_failure 'the generated rust-script workspace' "$workspace_manifest" "$output" \
        'build release executables for' || exit $?
fi
cat "$output"

test_json=$packages/tests.jsonl
if ! cargo test --no-run --message-format=json \
    --manifest-path "$workspace_manifest" "${test_package_args[@]}" \
    --target-dir "$build_target" >"$test_json" 2>"$output"; then
    # Cargo writes rendered compiler errors to JSON stdout in this mode, while
    # stderr may contain only the final error count. Retain the actual cause
    # before the generated package directory is removed by ordinary cleanup.
    if ! jq -r 'select(.reason == "compiler-message" and .message.level == "error")
        | (.message.rendered // .message.message)' "$test_json" >&2; then
        printf 'prepare-rust-scripts: could not decode compiler diagnostics from %s\n' "$test_json" >&2
    fi
    report_cargo_failure 'the generated rust-script workspace' "$workspace_manifest" "$output" \
        'build test harnesses for' || exit $?
fi
cat "$output"

for index in "${!entrypoints[@]}"; do
    source=${entrypoints[$index]}
    key=${keys[$index]}
    package_name=${package_names[$index]}
    run_source=$build_target/release/$package_name
    [[ -x $run_source ]] || {
        printf 'prepare-rust-scripts: release binary missing for %s: %s\n' "$source" "$run_source" >&2
        exit 2
    }
    run_rel=run/$key
    install -m 0755 "$run_source" "$scratch/$run_rel"
    strip "$scratch/$run_rel"

    test_rel=-
    if grep -q '#\[cfg(test)\]' -- "$source"; then
        mapfile -t test_binaries < <(jq -er --arg name "$package_name" \
            'select(.reason == "compiler-artifact" and .profile.test == true and .target.name == $name and .executable != null) | .executable' \
            "$test_json" | sort -u)
        ((${#test_binaries[@]} == 1)) || {
            printf 'prepare-rust-scripts: expected one test harness for %s, found %d\n' \
                "$source" "${#test_binaries[@]}" >&2
            exit 2
        }
        test_rel=test/$key
        install -m 0755 "${test_binaries[0]}" "$scratch/$test_rel"
        strip "$scratch/$test_rel"
    fi
    printf '%s\t%s\t%s\n' "$source" "$run_rel" "$test_rel" >>"$scratch/manifest.tsv"
done

write_state >"$state_after"
if ! cmp -s "$state_input" "$state_after"; then
    echo 'prepare-rust-scripts: source or tool state changed during the build; refusing to publish mixed artifacts' >&2
    exit 2
fi
printf '%s\n' "$state" >"$scratch/stamp"
printf '%s\n' "$($real_rust_script --version)" >"$scratch/rust-script-version"

if [[ -L $published || (-e $published && ! -d $published) ]]; then
    printf 'prepare-rust-scripts: refusing to replace non-directory output: %s\n' "$published" >&2
    exit 2
fi
rm -rf -- "$published"
mv -- "$scratch" "$published"
trap - EXIT
rm -f -- "$state_input" "$state_after"
rm -rf -- "$packages"

printf 'prepare-rust-scripts: built %d entrypoints and %d test harnesses in %s\n' \
    "${#entrypoints[@]}" "${#test_entrypoints[@]}" "$published"
