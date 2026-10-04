#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Exercise the actual dispatcher against independent Git fixture repositories.
set -euo pipefail
# Git exports its repository-location variables to hooks and `git rebase
# --exec` steps, and they override `git -C`. Every repository below is named
# explicitly, so run without them; otherwise a scratch `git init` rewrites the
# caller's repository (https://github.com/rrnewton/hermit/issues/3362).
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY \
    GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_PREFIX

SOURCE_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
scratch=$(mktemp -d -t hermit-rust-script-ownership.XXXXXXXX)
trap 'rm -rf -- "$scratch"' EXIT
root="$scratch/outer checkout"
published="$scratch/prepared"
mkdir -p "$root/ci/rust-script-bin" "$root/scripts" "$published" "$scratch/external" "$scratch/git-control"
cp "$SOURCE_DIR/rust-script" "$root/ci/rust-script-bin/rust-script"
cp "$SOURCE_DIR/run-test-harness" "$root/ci/rust-script-bin/run-test-harness"
wrapper="$root/ci/rust-script-bin/rust-script"
git -C "$root" init -q
listed="$root/scripts/listed source.rs"
unlisted="$root/scripts/unlisted.rs"
external="$scratch/external/source.rs"
printf '%s\n' 'fn main() {}' > "$listed"
cp "$listed" "$unlisted"
cp "$listed" "$external"
printf 'scripts/listed source.rs\trun\ttest\n' > "$published/manifest.tsv"
printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\0" prepared "$@"' > "$published/run"
printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\0" prepared-test "$@"' > "$published/test"
real_runner="$scratch/external/rust-script"
cat > "$real_runner" <<'RUNNER'
#!/usr/bin/env bash
printf '%s\0' delegated "$@" >> "$HERMIT_TEST_RUST_SCRIPT_CALLS"
printf '%s\0' delegated "$@"
RUNNER
chmod +x "$published/run" "$published/test" "$real_runner"

export HERMIT_REAL_RUST_SCRIPT="$real_runner"
export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$published"
export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1
export HERMIT_TEST_RUST_SCRIPT_CALLS="$scratch/delegated"
export DAGRUN_LOG_DIR="$scratch/runner-logs"
case_label='external and prepared sources'

invoke() {
    : > "$HERMIT_TEST_RUST_SCRIPT_CALLS"
    status=0
    "$wrapper" "$@" > "$scratch/stdout" 2> "$scratch/stderr" || status=$?
}
expect_output() {
    [[ $status == 0 ]] || {
        cat "$scratch/stderr" >&2
        echo "rust-script ownership: $case_label: expected success, got $status" >&2
        exit 1
    }
    printf '%s\0' "$@" > "$scratch/expected"
    cmp "$scratch/expected" "$scratch/stdout"
    [[ ! -s "$scratch/stderr" ]]
}
expect_refusal() {
    [[ $status == 2 ]] || {
        echo "rust-script ownership: $case_label: expected refusal, got $status" >&2
        exit 1
    }
    grep -Fq -- "$1" "$scratch/stderr"
    [[ ! -s "$scratch/stdout" && ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
}

# Original external/unlisted controls, with exact argv including empty strings.
invoke --force "$external" -- 'two words' '' 'literal $value'
expect_output delegated --force "$external" -- 'two words' '' 'literal $value'
cmp "$scratch/stdout" "$HERMIT_TEST_RUST_SCRIPT_CALLS"
invoke --force "$unlisted"
expect_refusal 'producer manifest has no unique entry for scripts/unlisted.rs'

# Sources owned by this checkout still execute only their prepared artifacts.
invoke --force "$listed" 'two words' ''
expect_output prepared 'two words' ''
[[ ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
invoke --test "$listed" --exact 'test name'
# A harness now reports its retained private log directory. Keep the original
# exact stdout/argv and empty remaining-stderr controls after checking that line.
test_logs=$(sed -n 's/^rust-script test runner logs: //p' "$scratch/stderr")
[[ -d $test_logs && $test_logs == "$DAGRUN_LOG_DIR"/script-test.* ]]
printf 'rust-script test runner logs: %s\n' "$test_logs" > "$scratch/expected-stderr"
cmp "$scratch/expected-stderr" "$scratch/stderr"
: > "$scratch/stderr"
expect_output prepared-test --exact 'test name'
[[ ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
ln -s "$listed" "$root/scripts/listed-link.rs"
invoke --force "$root/scripts/listed-link.rs" -- fixture
expect_output prepared -- fixture
[[ ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
ln -s "$root" "$scratch/outer-link"
wrapper="$scratch/outer-link/ci/rust-script-bin/rust-script"
invoke --force "$listed" -- fixture
expect_output prepared -- fixture
[[ ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
wrapper="$root/ci/rust-script-bin/rust-script"

# A separate Git owner can have either a .git directory or a .git file.
for layout in directory file; do
    case_label="nested Git $layout"
    nested="$root/target/nested-$layout"
    mkdir -p "$nested"
    if [[ $layout == directory ]]; then
        git -C "$nested" init -q
    else
        git -C "$nested" init -q --separate-git-dir="$scratch/separate-git-dir"
    fi
    cp "$listed" "$nested/source.rs"
    invoke --force "$nested/source.rs" -- 'two words' ''
    expect_output delegated --force "$nested/source.rs" -- 'two words' ''
    cmp "$scratch/stdout" "$HERMIT_TEST_RUST_SCRIPT_CALLS"
done

# Git exports GIT_DIR to `git rebase --exec` steps, and GIT_DIR, GIT_WORK_TREE
# and GIT_INDEX_FILE to hooks. The ownership lookup names the script's own
# directory, so an inherited location aimed at the caller's repository must not
# turn a listed source into a delegated one, refuse it, or touch the caller
# (https://github.com/rrnewton/hermit/issues/3362).
caller="$scratch/caller"
git init -q "$caller/main"
printf 'caller\n' > "$caller/main/tracked.txt"
git -C "$caller/main" add tracked.txt
git -C "$caller/main" -c user.email=fixture@example.invalid -c user.name=fixture \
    -c commit.gpgsign=false commit -qm caller
git -C "$caller/main" worktree add -q --detach "$caller/linked"
caller_git_dir="$caller/main/.git/worktrees/linked"
caller_state() {
    (cd "$caller" && find . -type f -print0 | sort -z | xargs -0 sha256sum &&
        find . -type f -printf '%P %i\n' | sort)
}
caller_before=$(caller_state)
invoke_inherited() {
    : > "$HERMIT_TEST_RUST_SCRIPT_CALLS"
    status=0
    (cd "$caller/linked" && env "${location[@]}" "$wrapper" "$@") \
        > "$scratch/stdout" 2> "$scratch/stderr" || status=$?
}
nested_source="$root/target/nested-directory/source.rs"
for shape in GIT_DIR GIT_WORK_TREE+GIT_INDEX_FILE GIT_DIR+GIT_WORK_TREE+GIT_INDEX_FILE; do
    case $shape in
        GIT_DIR) location=(GIT_DIR="$caller_git_dir") ;;
        GIT_WORK_TREE+GIT_INDEX_FILE)
            location=(GIT_WORK_TREE="$caller/linked" GIT_INDEX_FILE="$caller_git_dir/index") ;;
        GIT_DIR+GIT_WORK_TREE+GIT_INDEX_FILE)
            location=(GIT_DIR="$caller_git_dir" GIT_WORK_TREE="$caller/linked"
                GIT_INDEX_FILE="$caller_git_dir/index") ;;
    esac
    case_label="inherited $shape, listed source"
    invoke_inherited --force "$listed" -- fixture
    expect_output prepared -- fixture
    [[ ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
    case_label="inherited $shape, nested source"
    invoke_inherited --force "$nested_source" -- fixture
    expect_output delegated --force "$nested_source" -- fixture
    cmp "$scratch/stdout" "$HERMIT_TEST_RUST_SCRIPT_CALLS"
done
[[ $(caller_state) == "$caller_before" ]] || {
    echo "rust-script ownership: an inherited-location lookup changed the caller's repository" >&2
    exit 1
}

# An unsuccessful or malformed ownership lookup never authorizes compilation.
cat > "$scratch/git-control/git" <<'GIT'
#!/usr/bin/env bash
case $HERMIT_TEST_GIT_OWNER_RESULT in
    failure) exit 128 ;;
    empty) exit 0 ;;
    missing) printf '%s\n' "$HERMIT_TEST_GIT_MISSING_ROOT" ;;
    unrelated) printf '%s\n' "$HERMIT_TEST_GIT_UNRELATED_ROOT" ;;
    *) exit 99 ;;
esac
GIT
chmod +x "$scratch/git-control/git"
export HERMIT_TEST_GIT_MISSING_ROOT="$scratch/does-not-exist"
export HERMIT_TEST_GIT_UNRELATED_ROOT="$scratch/external"
for result in failure empty missing unrelated; do
    case_label="Git ownership $result"
    export HERMIT_TEST_GIT_OWNER_RESULT="$result"
    PATH="$scratch/git-control:$PATH" invoke --force "$unlisted"
    expect_refusal 'cannot establish Git ownership for repository script'
done

# Optional capability resolution preserves ownership/path refusals while giving
# absence its own status. It never executes either prepared or external code.
case_label='optional checked resolution'
invoke --resolve-optional --force "$listed"
[[ $status == 0 && $(<"$scratch/stdout") == "$published/run" && ! -s "$scratch/stderr" && ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
for help in --help -h; do
    invoke --resolve-optional "$help"
    [[ $status == 0 && ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
    grep -Fq '3 means absent' "$scratch/stdout"
done
invoke --resolve-optional --version
expect_refusal '--resolve-optional accepts a source path'
invoke --resolve-optional "$unlisted"
[[ $status == 3 && ! -s "$scratch/stdout" && ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
mv "$published/manifest.tsv" "$published/manifest.saved"
invoke --resolve-optional "$listed"
[[ $status == 3 && ! -s "$scratch/stdout" && ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
mv "$published/manifest.saved" "$published/manifest.tsv"
mv "$published/run" "$published/run.saved"
invoke --resolve-optional "$listed"
[[ $status == 3 && ! -s "$scratch/stdout" && ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
mkdir "$published/run"
invoke --resolve-optional "$listed"
expect_refusal 'not executable'
rmdir "$published/run"
mv "$published/run.saved" "$published/run"
chmod -x "$published/run"
invoke --resolve-optional "$listed"
expect_refusal 'not executable'
chmod +x "$published/run"
cp "$published/manifest.tsv" "$published/manifest.saved"
for kind in duplicate malformed absolute escaping; do
    case_label="optional $kind"
    case $kind in
        duplicate) cat "$published/manifest.saved" "$published/manifest.saved" > "$published/manifest.tsv" ;;
        malformed) printf 'scripts/listed source.rs\trun\n' > "$published/manifest.tsv" ;;
        absolute) printf 'scripts/listed source.rs\t/absent\ttest\n' > "$published/manifest.tsv" ;;
        escaping) printf 'scripts/listed source.rs\t./../absent\ttest\n' > "$published/manifest.tsv" ;;
    esac
    invoke --resolve-optional "$listed"
    [[ $status == 2 && ! -s "$scratch/stdout" && ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
done
mv "$published/manifest.saved" "$published/manifest.tsv"
for candidate in "$external" "$root/target/nested-directory/source.rs"; do
    invoke --resolve-optional "$candidate"
    [[ $status == 2 && ! -s "$scratch/stdout" && ! -s "$HERMIT_TEST_RUST_SCRIPT_CALLS" ]]
done
for result in failure empty missing unrelated; do
    case_label="optional Git ownership $result"
    export HERMIT_TEST_GIT_OWNER_RESULT="$result"
    PATH="$scratch/git-control:$PATH" invoke --resolve-optional "$unlisted"
    expect_refusal 'cannot establish Git ownership for repository script'
done

echo 'rust-script ownership: prepared, external, nested, inherited-location, and refusal controls passed'
