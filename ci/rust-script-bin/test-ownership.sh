#!/usr/bin/env bash
# Exercise the actual dispatcher against independent Git fixture repositories.
set -euo pipefail

SOURCE_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
scratch=$(mktemp -d /tmp/hermit-rust-script-ownership.XXXXXXXX)
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

echo 'rust-script ownership: prepared, external, nested, and refusal controls passed'
