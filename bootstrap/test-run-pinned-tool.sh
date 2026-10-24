#!/usr/bin/env bash
# Regression test for bootstrap/run-pinned-tool under an inherited repository
# location.
#
# Git exports GIT_DIR to `git rebase --exec` steps, and GIT_DIR, GIT_WORK_TREE
# and GIT_INDEX_FILE to hooks. They override `git -C`, so on a cache miss the
# tool's `git -C <cache> init`, `fetch` and `checkout --detach` used to act on
# the caller's repository (https://github.com/rrnewton/hermit/issues/3362).
# Each case runs a cache miss against a local upstream and a fake rustup, with
# the variables aimed at a throwaway caller repository, and requires the cache
# checkout at the pinned revision, the build to see none of the variables, the
# tool itself to keep the caller's environment, and the caller's repository to
# stay byte- and inode-identical.
set -euo pipefail

location_variables=(GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR
  GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_PREFIX)
# This test's own fixture commands must not follow the caller either.
unset "${location_variables[@]}"
# Each case names its own cache. A validation service exports
# HERMIT_BUCK2_SHARED_TOOL_CACHE to every step, this test included, and it would
# otherwise win over the cases' caches and fill the real one with fixtures.
unset HERMIT_BUCK2_SHARED_TOOL_CACHE HERMIT_BUCK2_TOOL_CACHE XDG_CACHE_HOME

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
tool_script=$script_dir/run-pinned-tool
[[ -x $tool_script ]] || { echo "test-run-pinned-tool: missing $tool_script" >&2; exit 2; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fixture_git() {
  git -c user.email=fixture@example.invalid -c user.name=fixture \
    -c commit.gpgsign=false -c init.defaultBranch=main "$@"
}

# A local upstream standing in for the Reindeer repository.
upstream=$tmp/upstream
fixture_git init -q "$upstream"
printf '[package]\nname = "reindeer"\n' >"$upstream/Cargo.toml"
fixture_git -C "$upstream" add Cargo.toml
fixture_git -C "$upstream" commit -qm upstream
fixture_git -C "$upstream" config uploadpack.allowAnySHA1InWant true
revision=$(git -C "$upstream" rev-parse HEAD)
build_key=reindeer-$revision-fixture-toolchain-r1

# The script under test, beside pins that name the local upstream.
mkdir "$tmp/bootstrap"
cp "$tool_script" "$tmp/bootstrap/run-pinned-tool"
cat >"$tmp/bootstrap/tool-pins.sh" <<EOF
REINDEER_REPOSITORY=file://$upstream
REINDEER_REVISION=$revision
REINDEER_RUST_TOOLCHAIN=fixture-toolchain
EOF

# A fake rustup that records the build's view of the location variables and
# produces a tool that reports its own view. It and a git wrapper record every
# command that inherits the lock's descriptor.
mkdir "$tmp/bin"
cat >"$tmp/bin/rustup" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ -e /proc/self/fd/9 ]] && echo "fd9 open: rustup $*" >>"$FAKE_RUSTUP_LOG"
if [[ $1 == toolchain ]]; then
  [[ $2 == install ]] || exit 64
  echo "install $3" >>"$FAKE_RUSTUP_LOG"
  exit 0
fi
[[ $1 == run ]] || exit 64
shift 2
case $1 in
  rustc)
    [[ -z ${FAKE_RUSTUP_NO_TOOLCHAIN:-} ]] || exit 1
    echo "rustc fixture"
    ;;
  cargo)
    printf 'build GIT_DIR=%s GIT_WORK_TREE=%s GIT_INDEX_FILE=%s\n' \
      "${GIT_DIR-<unset>}" "${GIT_WORK_TREE-<unset>}" "${GIT_INDEX_FILE-<unset>}" \
      >>"$FAKE_RUSTUP_LOG"
    fd9=closed
    [[ -e /proc/self/fd/9 ]] && fd9=open
    printf 'env RUSTFLAGS=%s CARGO_ENCODED_RUSTFLAGS=%s RUSTC_WRAPPER=%s CARGO_PROFILE_RELEASE_LTO=%s CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=%s fd9=%s\n' \
      "${RUSTFLAGS-<unset>}" "${CARGO_ENCODED_RUSTFLAGS-<unset>}" "${RUSTC_WRAPPER-<unset>}" \
      "${CARGO_PROFILE_RELEASE_LTO-<unset>}" "${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER-<unset>}" "$fd9" \
      >>"$FAKE_RUSTUP_LOG"
    sleep "${FAKE_RUSTUP_BUILD_SECONDS:-0}"
    mkdir -p "$CARGO_TARGET_DIR/release"
    cat >"$CARGO_TARGET_DIR/release/reindeer" <<'TOOL'
#!/usr/bin/env bash
printf 'fixture reindeer %s GIT_DIR=%s\n' "$*" "${GIT_DIR-<unset>}"
TOOL
    chmod +x "$CARGO_TARGET_DIR/release/reindeer"
    ;;
  *) exit 64 ;;
esac
EOF
chmod +x "$tmp/bin/rustup"
cat >"$tmp/bin/git" <<EOF
#!/usr/bin/env bash
if [[ -e /proc/self/fd/9 && -n \${FAKE_RUSTUP_LOG:-} ]]; then
  echo "fd9 open: git \$*" >>"\$FAKE_RUSTUP_LOG"
fi
exec $(command -v git) "\$@"
EOF
chmod +x "$tmp/bin/git"

# The caller: a repository with a linked worktree, as a hook would see it.
caller=$tmp/caller
fixture_git init -q "$caller/main"
printf 'caller\n' >"$caller/main/tracked.txt"
fixture_git -C "$caller/main" add tracked.txt
fixture_git -C "$caller/main" commit -qm caller
fixture_git -C "$caller/main" worktree add -q --detach "$caller/linked"
caller_git_dir=$caller/main/.git/worktrees/linked

snapshot() {
  (
    cd "$caller"
    find . -type f -print0 | sort -z | xargs -0 sha256sum
    find . -type f -printf '%P %i\n' | sort
  )
}

failures=0
case_number=0
run_case() {
  local label=$1
  shift
  case_number=$((case_number + 1))
  local cache=$tmp/cache-$case_number
  local log=$tmp/rustup-$case_number.log
  local before after output
  before=$(snapshot)
  if ! output=$(cd "$caller/linked" && env "$@" PATH="$tmp/bin:$PATH" \
      HERMIT_BUCK2_TOOL_CACHE="$cache" FAKE_RUSTUP_LOG="$log" \
      "$tmp/bootstrap/run-pinned-tool" reindeer --fixture-argument 2>&1); then
    echo "FAIL [$label]: run-pinned-tool exited nonzero:" >&2
    printf '%s\n' "$output" >&2
    failures=$((failures + 1))
    return
  fi
  after=$(snapshot)
  if [[ $before != "$after" ]]; then
    echo "FAIL [$label]: the caller's repository changed:" >&2
    diff <(printf '%s\n' "$before") <(printf '%s\n' "$after") >&2 || true
    failures=$((failures + 1))
  fi
  local cached
  cached=$(git -C "$cache/source/$build_key" rev-parse HEAD 2>&1) || true
  if [[ $cached != "$revision" ]]; then
    echo "FAIL [$label]: cache checkout is at '$cached', expected $revision" >&2
    failures=$((failures + 1))
  fi
  if ! grep -qx 'build GIT_DIR=<unset> GIT_WORK_TREE=<unset> GIT_INDEX_FILE=<unset>' "$log"; then
    echo "FAIL [$label]: the cache build saw an inherited location: $(cat "$log" 2>&1)" >&2
    failures=$((failures + 1))
  fi
  local expected_tool_git_dir=${GIT_DIR_FOR_CASE:-<unset>}
  if ! grep -qxF "fixture reindeer --fixture-argument GIT_DIR=$expected_tool_git_dir" <<<"$output"; then
    echo "FAIL [$label]: the tool did not run with the caller's environment:" >&2
    printf '%s\n' "$output" >&2
    failures=$((failures + 1))
  fi
}

GIT_DIR_FOR_CASE=$caller_git_dir run_case "GIT_DIR (rebase --exec)" \
  GIT_DIR="$caller_git_dir"
GIT_DIR_FOR_CASE='' run_case "GIT_WORK_TREE and GIT_INDEX_FILE" \
  GIT_WORK_TREE="$caller/linked" GIT_INDEX_FILE="$caller_git_dir/index"
GIT_DIR_FOR_CASE=$caller_git_dir run_case "GIT_DIR, GIT_WORK_TREE and GIT_INDEX_FILE (hook)" \
  GIT_DIR="$caller_git_dir" GIT_WORK_TREE="$caller/linked" GIT_INDEX_FILE="$caller_git_dir/index"

# The cache is meant to outlive a checkout, so a validation service can share
# one across runs. These cases run without inherited locations.
builds() {
  [[ -e $1 ]] || { echo 0; return; }
  grep -c '^build ' "$1" || true
}
run_tool() {
  local cache=$1 log=$2
  shift 2
  env "$@" PATH="$tmp/bin:$PATH" HERMIT_BUCK2_TOOL_CACHE="$cache" FAKE_RUSTUP_LOG="$log" \
    "${script:-$tmp/bootstrap/run-pinned-tool}" reindeer --fixture-argument
}
expect_tool_output() {
  local label=$1 output=$2
  if ! grep -qxF "fixture reindeer --fixture-argument GIT_DIR=<unset>" <<<"$output"; then
    echo "FAIL [$label]: the tool did not run:" >&2
    printf '%s\n' "$output" >&2
    failures=$((failures + 1))
  fi
}
expect_builds() {
  local label=$1 log=$2 expected=$3 actual
  actual=$(builds "$log")
  if [[ $actual != "$expected" ]]; then
    echo "FAIL [$label]: $actual build(s), expected $expected" >&2
    failures=$((failures + 1))
  fi
}

# A hit runs the cached binary without building.
cache=$tmp/cache-hit
log=$tmp/rustup-hit.log
for attempt in 1 2; do
  output=$(run_tool "$cache" "$log" 2>&1) || true
  expect_tool_output "hit, run $attempt" "$output"
done
expect_builds "a hit does not build" "$log" 1

# A checkout left by an interrupted miss, here one whose remote is wrong, is
# replaced rather than resumed.
cache=$tmp/cache-interrupted
log=$tmp/rustup-interrupted.log
fixture_git init -q "$cache/source/$build_key"
fixture_git -C "$cache/source/$build_key" remote add origin "file://$tmp/no-such-upstream"
output=$(run_tool "$cache" "$log" 2>&1) || true
expect_tool_output "interrupted checkout" "$output"
expect_builds "interrupted checkout" "$log" 1
cached=$(git -C "$cache/source/$build_key" rev-parse HEAD 2>&1) || true
if [[ $cached != "$revision" ]]; then
  echo "FAIL [interrupted checkout]: cache checkout is at '$cached', expected $revision" >&2
  failures=$((failures + 1))
fi

# Concurrent misses on one cache build once; each runs the tool.
cache=$tmp/cache-concurrent
log=$tmp/rustup-concurrent.log
pids=()
for runner in 1 2 3 4; do
  run_tool "$cache" "$log" FAKE_RUSTUP_BUILD_SECONDS=2 >"$tmp/concurrent-$runner.out" 2>&1 &
  pids+=($!)
done
for runner in 1 2 3 4; do
  if ! wait "${pids[runner - 1]}"; then
    echo "FAIL [concurrent misses]: runner $runner exited nonzero:" >&2
    cat "$tmp/concurrent-$runner.out" >&2
    failures=$((failures + 1))
  fi
  expect_tool_output "concurrent misses, runner $runner" "$(cat "$tmp/concurrent-$runner.out")"
done
expect_builds "concurrent misses" "$log" 1

# A second checkout pinning another toolchain at the same revision.
mkdir "$tmp/bootstrap-2"
cp "$tmp/bootstrap/run-pinned-tool" "$tmp/bootstrap-2/run-pinned-tool"
sed 's/^REINDEER_RUST_TOOLCHAIN=.*/REINDEER_RUST_TOOLCHAIN=fixture-toolchain-2/' \
  "$tmp/bootstrap/tool-pins.sh" >"$tmp/bootstrap-2/tool-pins.sh"

# A new toolchain pin at the same revision builds again.
cache=$tmp/cache-toolchain
log=$tmp/rustup-toolchain.log
output=$(run_tool "$cache" "$log" 2>&1) || true
expect_tool_output "first toolchain" "$output"
output=$(script=$tmp/bootstrap-2/run-pinned-tool run_tool "$cache" "$log" 2>&1) || true
expect_tool_output "second toolchain" "$output"
expect_builds "a new toolchain pin" "$log" 2

# Concurrent misses for two toolchains at one revision do not share a checkout.
cache=$tmp/cache-two-toolchains
log=$tmp/rustup-two-toolchains.log
for attempt in 1 2 3; do
  rm -rf -- "${cache:?}"
  run_tool "$cache" "$log" FAKE_RUSTUP_BUILD_SECONDS=1 >"$tmp/toolchain-1.out" 2>&1 &
  first=$!
  script=$tmp/bootstrap-2/run-pinned-tool run_tool "$cache" "$log" FAKE_RUSTUP_BUILD_SECONDS=1 \
    >"$tmp/toolchain-2.out" 2>&1 &
  second=$!
  for runner in 1 2; do
    pid=$first
    [[ $runner == 2 ]] && pid=$second
    if ! wait "$pid"; then
      echo "FAIL [two toolchains, attempt $attempt]: runner $runner exited nonzero:" >&2
      cat "$tmp/toolchain-$runner.out" >&2
      failures=$((failures + 1))
    fi
  done
done
expect_builds "two toolchains, 3 attempts" "$log" 6

# A hit takes no lock: it runs while another process holds the build's lock.
cache=$tmp/cache-hit-locked
log=$tmp/rustup-hit-locked.log
output=$(run_tool "$cache" "$log" 2>&1) || true
expect_tool_output "hit under a held lock, first run" "$output"
# The holder execs into sleep, so killing it releases the lock and leaves
# nothing holding this test's output open.
(
  exec 8>"$cache/$build_key.lock"
  flock 8
  exec sleep 30
) </dev/null >/dev/null 2>&1 &
holder=$!
sleep 0.2
if ! output=$(timeout 10 env PATH="$tmp/bin:$PATH" HERMIT_BUCK2_TOOL_CACHE="$cache" \
    FAKE_RUSTUP_LOG="$log" "$tmp/bootstrap/run-pinned-tool" reindeer --fixture-argument 2>&1); then
  echo "FAIL [hit under a held lock]: the hit did not finish: $output" >&2
  failures=$((failures + 1))
fi
expect_tool_output "hit under a held lock" "$output"
kill "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true

# The miss closes the lock's descriptor in its commands, the toolchain install
# included, and drops the caller's flag, wrapper, profile and target settings,
# which are not part of the build key.
cache=$tmp/cache-environment
log=$tmp/rustup-environment.log
output=$(run_tool "$cache" "$log" FAKE_RUSTUP_NO_TOOLCHAIN=1 RUSTFLAGS=-Dwarnings \
  CARGO_ENCODED_RUSTFLAGS=-Dwarnings RUSTC_WRAPPER=/no/such/wrapper \
  CARGO_PROFILE_RELEASE_LTO=fat CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/no/such/linker 2>&1) || true
expect_tool_output "caller compiler settings" "$output"
if ! grep -qx 'env RUSTFLAGS=<unset> CARGO_ENCODED_RUSTFLAGS=<unset> RUSTC_WRAPPER=<unset> CARGO_PROFILE_RELEASE_LTO=<unset> CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=<unset> fd9=closed' "$log"; then
  echo "FAIL [build environment]: $(grep '^env ' "$log" 2>&1)" >&2
  failures=$((failures + 1))
fi
if ! grep -qx 'install fixture-toolchain' "$log"; then
  echo "FAIL [toolchain install]: the miss did not install the missing toolchain: $(cat "$log")" >&2
  failures=$((failures + 1))
fi
if grep -h '^fd9 open: ' "$tmp"/rustup-*.log >&2; then
  echo "FAIL [lock descriptor]: the commands above inherited the lock's descriptor" >&2
  failures=$((failures + 1))
fi

# HERMIT_BUCK2_SHARED_TOOL_CACHE wins over HERMIT_BUCK2_TOOL_CACHE.
shared=$tmp/cache-shared
log=$tmp/rustup-shared.log
output=$(run_tool "$tmp/cache-not-shared" "$log" HERMIT_BUCK2_SHARED_TOOL_CACHE="$shared" 2>&1) || true
expect_tool_output "shared cache" "$output"
if [[ ! -x $shared/target/$build_key/release/reindeer || -e $tmp/cache-not-shared ]]; then
  echo "FAIL [shared cache]: the build did not go to HERMIT_BUCK2_SHARED_TOOL_CACHE" >&2
  failures=$((failures + 1))
fi

# Without flock a miss stops and says so.
mkdir "$tmp/no-flock-bin"
for command in bash env git dirname mkdir rm cat grep sed sleep; do
  ln -s "$(command -v "$command")" "$tmp/no-flock-bin/$command"
done
ln -s "$tmp/bin/rustup" "$tmp/no-flock-bin/rustup"
if output=$(env PATH="$tmp/no-flock-bin" HERMIT_BUCK2_TOOL_CACHE="$tmp/cache-no-flock" \
    FAKE_RUSTUP_LOG="$tmp/rustup-no-flock.log" "$tmp/bootstrap/run-pinned-tool" reindeer 2>&1); then
  echo "FAIL [no flock]: the miss succeeded without flock" >&2
  failures=$((failures + 1))
elif ! grep -qx 'flock is required' <<<"$output"; then
  echo "FAIL [no flock]: unexpected refusal: $output" >&2
  failures=$((failures + 1))
fi

# The whole test again under an inherited HERMIT_BUCK2_SHARED_TOOL_CACHE, as a
# validation step runs it; nothing may reach that cache.
if [[ -z ${TEST_RUN_PINNED_TOOL_NESTED:-} ]]; then
  if ! output=$(TEST_RUN_PINNED_TOOL_NESTED=1 HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/inherited-shared" \
      "$script_dir/test-run-pinned-tool.sh" 2>&1); then
    echo "FAIL [inherited shared cache]: the test fails under HERMIT_BUCK2_SHARED_TOOL_CACHE:" >&2
    printf '%s\n' "$output" >&2
    failures=$((failures + 1))
  elif [[ -e $tmp/inherited-shared ]]; then
    echo "FAIL [inherited shared cache]: the test wrote to the inherited cache" >&2
    failures=$((failures + 1))
  fi
fi

if [[ $failures -ne 0 ]]; then
  echo "test-run-pinned-tool: $failures failure(s)" >&2
  exit 1
fi
echo "test-run-pinned-tool: OK - $case_number inherited-location cache misses left the caller untouched; the shared-cache cases passed"
