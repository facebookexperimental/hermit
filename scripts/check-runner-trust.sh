#!/usr/bin/env bash
# check-runner-trust.sh — self-hosted runner trust guard.
#
# Why bash, not a rust-script: this runs as the step immediately after
# actions/checkout and before any build/toolchain step on a self-hosted
# runner. At that point rust-script is not guaranteed to be installed, so
# the guard must depend only on bash + git, which checkout itself requires.
# The pre-checkout actor guard cannot use this file at all (no repo is
# checked out yet); it is inlined in each workflow YAML. This script is
# the post-checkout half.
#
# Trust rule (owner directive, 2026-10-31):
#   * The GitHub actor who triggered the run must be allow-listed.
#   * Every commit in base..HEAD must be AUTHORED by an allow-listed
#     identity. The author identity decides; the committer is logged for
#     audit but does not decide (a trusted rebase/cherry-pick of an
#     outside author's commit still refuses, and an outside committer
#     applying a trusted author's patch is noted, not refused).
#   * Merge commits are commits: their author is checked like any other.
#     There is no merge exemption.
#   * If the commit range cannot be established, refuse. An empty range
#     (HEAD is the base tip, e.g. a schedule run on main) passes.
#
# Base-range rule:
#   * --base <rev> / $RUNNER_TRUST_BASE if given, else origin/main, else
#     main. The range is merge-base(base, HEAD)..HEAD, so a dispatched
#     feature branch is judged on exactly the commits it adds on top of
#     main, and a workflow_dispatch on main itself yields an empty range.
#   * If origin/main is absent the script tries `git fetch origin main`
#     once; if the base still cannot be resolved, or merge-base fails,
#     it refuses rather than guessing.
#
# Usage:
#   scripts/check-runner-trust.sh [--actor NAME] [--base REV] [--head REV]
#   scripts/check-runner-trust.sh --self-test
# Actor defaults to $GITHUB_ACTOR. Exit 0 = trusted, 1 = refused.
set -euo pipefail

ALLOWED_ACTORS=("hermit-bot3" "rrnewton")
# Author identities are matched by email, not display name: names are
# free text and spoofable, while the email is the recorded identity.
# Defaults are the author emails observed on origin/main: the bot
# account (github-usage skill, 2026-10-18) and the repository owner's
# direct and GitHub-noreply addresses. Operators can extend the list
# without editing this file via $RUNNER_TRUST_ALLOWED_AUTHOR_EMAILS
# (comma- or space-separated); workflow jobs may source it from a
# repository variable so no personal address is hard-coded here.
ALLOWED_AUTHOR_EMAILS=(
  "344587749+hermit-bot3@users.noreply.github.com"
  "rrnewton@gmail.com"
  "rrnewton@users.noreply.github.com"
)
if [ -n "${RUNNER_TRUST_ALLOWED_AUTHOR_EMAILS:-}" ]; then
  extra="${RUNNER_TRUST_ALLOWED_AUTHOR_EMAILS//,/ }"
  # shellcheck disable=SC2206 # deliberate word splitting of the override list
  ALLOWED_AUTHOR_EMAILS+=($extra)
fi

actor="${GITHUB_ACTOR:-}"
base_arg="${RUNNER_TRUST_BASE:-}"
head_arg="HEAD"

usage() { echo "usage: $0 [--actor NAME] [--base REV] [--head REV] | --self-test" >&2; }

in_list() { local needle="$1"; shift; local x; for x in "$@"; do [ "$x" = "$needle" ] && return 0; done; return 1; }

check_actor() {
  if [ -z "$actor" ]; then
    echo "::error::runner-trust: no actor supplied (--actor or GITHUB_ACTOR); refusing" >&2; return 1
  fi
  if in_list "$actor" "${ALLOWED_ACTORS[@]}"; then
    echo "runner-trust: actor '$actor' is allow-listed"
  else
    echo "::error::runner-trust: actor '$actor' is not allow-listed for self-hosted runners (allowlist: ${ALLOWED_ACTORS[*]})" >&2; return 1
  fi
}

check_authors() {
  local head base merge_base line author committer sha
  head="$(git rev-parse --verify "${head_arg}^{commit}" 2>/dev/null)" || {
    echo "::error::runner-trust: cannot resolve HEAD rev '$head_arg'; refusing" >&2; return 1; }
  base=""
  if [ -n "$base_arg" ]; then
    base="$(git rev-parse --verify "${base_arg}^{commit}" 2>/dev/null)" || {
      echo "::error::runner-trust: cannot resolve --base '$base_arg'; refusing" >&2; return 1; }
  else
    if ! git rev-parse --verify --quiet "origin/main^{commit}" >/dev/null; then
      git fetch --no-tags origin main >/dev/null 2>&1 || true
    fi
    for cand in origin/main main; do
      if git rev-parse --verify --quiet "${cand}^{commit}" >/dev/null; then
        base="$(git rev-parse "${cand}^{commit}")"; echo "runner-trust: base is $cand ($base)"; break
      fi
    done
    [ -n "$base" ] || { echo "::error::runner-trust: cannot establish base (no origin/main or main); refusing" >&2; return 1; }
  fi
  merge_base="$(git merge-base "$base" "$head")" || {
    echo "::error::runner-trust: no merge-base between base $base and HEAD $head; refusing" >&2; return 1; }
  echo "runner-trust: checking authors in ${merge_base}..${head} (author decides; committer is audit-only)"
  local failures=0 count=0
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    count=$((count + 1))
    sha="${line%% *}"; author="${line#* }"; author="${author%%|*}"; committer="${line##*|}"
    author_email="${author##*<}"; author_email="${author_email%%>*}"
    if in_list "$author_email" "${ALLOWED_AUTHOR_EMAILS[@]}"; then
      echo "runner-trust: ok $sha author=$author committer=$committer"
    else
      echo "::error::runner-trust: commit $sha authored by '$author' is not allow-listed (committer=$committer)" >&2
      failures=$((failures + 1))
    fi
  done < <(git log --format='%H %an <%ae>|%cn <%ce>' "${merge_base}..${head}")
  if [ "$count" -eq 0 ]; then echo "runner-trust: empty commit range (HEAD is at the base tip); nothing new to execute"; fi
  [ "$failures" -eq 0 ] || { echo "::error::runner-trust: $failures untrusted author(s) in range; refusing to run on a self-hosted runner" >&2; return 1; }
  echo "runner-trust: all $count commit author(s) in range are allow-listed"
}

SCRIPT_PATH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/$(basename -- "${BASH_SOURCE[0]}")"

self_test() {
  local tmp failures=0
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/runner-trust-test.XXXXXX")"
  trap 'rm -rf "$tmp"' RETURN
  local repo="$tmp/repo"
  git init -q -b main "$repo"
  commit_as() { # name email msg [committer-name committer-email]
    local an="$1" ae="$2" msg="$3" cn="${4:-$1}" ce="${5:-$2}"
    echo "$msg $(date +%s%N)" >> "$repo/f.txt"
    git -C "$repo" add f.txt
    GIT_AUTHOR_NAME="$an" GIT_AUTHOR_EMAIL="$ae" GIT_COMMITTER_NAME="$cn" GIT_COMMITTER_EMAIL="$ce" \
      git -C "$repo" commit -q -m "$msg"
  }
  git -C "$repo" config user.name "hermit-bot3"
  git -C "$repo" config user.email "344587749+hermit-bot3@users.noreply.github.com"
  commit_as "hermit-bot3" "344587749+hermit-bot3@users.noreply.github.com" "base"
  git -C "$repo" branch base-tip
  guard() { bash "$SCRIPT_PATH" --actor "$1" --base base-tip --head HEAD; }
  expect() { # name want-exit actual-exit
    if [ "$2" = "$3" ]; then echo "self-test PASS: $1"; else echo "self-test FAIL: $1 (want exit $2, got $3)" >&2; failures=$((failures + 1)); fi
  }
  (
    cd "$repo"
    # 1. allow-listed actor + allow-listed authors passes
    commit_as "Repo Owner" "rrnewton@gmail.com" "owner work"
    guard hermit-bot3 >/dev/null 2>&1 && rc=0 || rc=$?; expect "allowlisted actor+authors passes" 0 "$rc"
    # 2. outside author in range refuses (even with trusted committer)
    commit_as "Outside Dev" "outside@example.com" "outside work" "hermit-bot3" "344587749+hermit-bot3@users.noreply.github.com"
    guard hermit-bot3 >/dev/null 2>&1 && rc=0 || rc=$?; expect "outside author refuses" 1 "$rc"
    git reset -q --hard HEAD~1
    # 3. non-allowlisted actor refuses
    guard outside-dev >/dev/null 2>&1 && rc=0 || rc=$?; expect "non-allowlisted actor refuses" 1 "$rc"
    # 4. empty range (schedule on main: HEAD == base tip) passes
    git checkout -q base-tip
    guard rrnewton >/dev/null 2>&1 && rc=0 || rc=$?; expect "empty range passes with allowlisted actor" 0 "$rc"
  )
  if [ "$failures" -eq 0 ]; then echo "runner-trust self-test: all cases passed"; else echo "runner-trust self-test: $failures failure(s)" >&2; return 1; fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --actor) actor="$2"; shift 2 ;;
    --base) base_arg="$2"; shift 2 ;;
    --head) head_arg="$2"; shift 2 ;;
    --self-test) self_test; exit $? ;;
    -h|--help) usage; exit 0 ;;
    *) usage; echo "::error::runner-trust: unknown argument '$1'" >&2; exit 1 ;;
  esac
done

check_actor
check_authors
echo "runner-trust: trusted; proceeding on self-hosted runner"
