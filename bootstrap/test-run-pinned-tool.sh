#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

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

# The script under test, beside pins that name the local upstream.
mkdir "$tmp/bootstrap"
cp "$tool_script" "$tmp/bootstrap/run-pinned-tool"
cat >"$tmp/bootstrap/tool-pins.sh" <<EOF
REINDEER_REPOSITORY=file://$upstream
REINDEER_REVISION=$revision
REINDEER_RUST_TOOLCHAIN=fixture-toolchain
EOF

# A fake rustup that records the build's view of the location variables and
# produces a tool that reports its own view.
mkdir "$tmp/bin"
cat >"$tmp/bin/rustup" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == run ]] || exit 64
shift 2
case $1 in
  rustc) echo "rustc fixture" ;;
  cargo)
    printf 'build GIT_DIR=%s GIT_WORK_TREE=%s GIT_INDEX_FILE=%s\n' \
      "${GIT_DIR-<unset>}" "${GIT_WORK_TREE-<unset>}" "${GIT_INDEX_FILE-<unset>}" \
      >>"$FAKE_RUSTUP_LOG"
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
  cached=$(git -C "$cache/source/reindeer-$revision" rev-parse HEAD 2>&1) || true
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

if [[ $failures -ne 0 ]]; then
  echo "test-run-pinned-tool: $failures failure(s)" >&2
  exit 1
fi
echo "test-run-pinned-tool: OK - $case_number inherited-location cache misses left the caller untouched"
