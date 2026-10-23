#!/usr/bin/env bash
# Regression test for bootstrap/git-dep-mirrors.sh (HERMIT_GIT_DEP_MIRRORS).
#
# Accept: a lockfile with two git sources (one URL with a .git suffix, one
# without) served from complete mirrors exports one insteadOf entry each,
# appended after the caller's own GIT_CONFIG_* entries, and the git executable
# then resolves each locked URL to its mirror. Refuse: a missing mirror, a mirror
# without a pinned commit, two sources that would share a mirror, a lockfile with
# no git sources and a mirror directory
# that does not exist each fail and export no GIT_CONFIG_COUNT. Coverage: run
# against the repository's own Cargo.lock with an empty directory, the refusal
# names every git source it locks, so no source format slips past the parser.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
helper=$script_dir/git-dep-mirrors.sh
[[ -f $helper ]] || { echo "test-git-dep-mirrors: missing $helper" >&2; exit 2; }

tmp=$(mktemp -d)
trap 'rm -rf "${tmp:?}"' EXIT
failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }

fixture_git() {
  git -c user.email=fixture@example.invalid -c user.name=fixture \
    -c commit.gpgsign=false -c init.defaultBranch=main "$@"
}
make_upstream() {
  fixture_git init -q "$1"
  echo "$2" >"$1/file"
  fixture_git -C "$1" add file
  fixture_git -C "$1" commit -qm "$2"
  git -C "$1" rev-parse HEAD
}

rev_a=$(make_upstream "$tmp/up/a" a)
rev_b1=$(make_upstream "$tmp/up/b" b1)
echo b2 >"$tmp/up/b/file"
fixture_git -C "$tmp/up/b" commit -qam b2
rev_b2=$(git -C "$tmp/up/b" rev-parse HEAD)
url_a=https://example.invalid/org/a.git
url_b=https://example.invalid/org/b
cat >"$tmp/Cargo.lock" <<EOF
[[package]]
name = "a-one"
source = "git+$url_a?branch=main#$rev_a"

[[package]]
name = "a-two"
source = "git+$url_a?branch=main#$rev_a"

[[package]]
name = "b-old"
source = "git+$url_b?rev=$rev_b1#$rev_b1"

[[package]]
name = "b-new"
source = "git+$url_b?rev=$rev_b2#$rev_b2"

[[package]]
name = "from-registry"
source = "registry+https://github.com/rust-lang/crates.io-index"
EOF
mirrors=$tmp/mirrors
mkdir -p "$mirrors"
git clone -q --mirror "$tmp/up/a" "$mirrors/a.git"
git clone -q --mirror "$tmp/up/b" "$mirrors/b.git"

# Runs git_dep_mirrors_apply in a clean child; prints its rc, then the exported
# GIT_CONFIG_* and CARGO_NET_* variables, then its stderr.
apply() {
  env -i PATH="$PATH" HOME="$tmp" "$@" bash -c '
    source "$1"
    rc=0
    git_dep_mirrors_apply "$2" "$3" 2>"$4" || rc=$?
    echo "rc=$rc"
    env | grep -E "^(GIT_CONFIG_|CARGO_NET_)" | sort
    cat "$4"' apply "$helper" "$lockfile" "$dir" "$tmp/stderr"
}

# Accept, appended to one caller entry.
lockfile=$tmp/Cargo.lock dir=$mirrors
out=$(apply GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.abbrev GIT_CONFIG_VALUE_0=12)
grep -qx 'rc=0' <<<"$out" || fail "accept: $out"
grep -qx 'GIT_CONFIG_COUNT=3' <<<"$out" || fail "accept: count is not 1 + 2 sources: $out"
grep -qx 'GIT_CONFIG_KEY_0=core.abbrev' <<<"$out" || fail "accept: caller entry lost: $out"
grep -qx "GIT_CONFIG_KEY_1=url.$mirrors/a.git.insteadOf" <<<"$out" || fail "accept: a key: $out"
grep -qx "GIT_CONFIG_VALUE_1=$url_a" <<<"$out" || fail "accept: a value: $out"
grep -qx "GIT_CONFIG_KEY_2=url.$mirrors/b.git.insteadOf" <<<"$out" || fail "accept: b key: $out"
grep -qx "GIT_CONFIG_VALUE_2=$url_b" <<<"$out" || fail "accept: b value: $out"
grep -qx 'CARGO_NET_GIT_FETCH_WITH_CLI=true' <<<"$out" || fail "accept: cargo would use its built-in git: $out"

# The redirect works for the git executable, for both URL shapes.
resolved=$(env -i PATH="$PATH" HOME="$tmp" bash -c '
  source "$1"; git_dep_mirrors_apply "$2" "$3" 2>/dev/null
  git ls-remote "$4" HEAD; git ls-remote "$5" HEAD' _ "$helper" "$tmp/Cargo.lock" "$mirrors" "$url_a" "$url_b")
grep -q "^$rev_a	HEAD$" <<<"$resolved" || fail "git did not resolve $url_a to its mirror: $resolved"
grep -q "^$rev_b2	HEAD$" <<<"$resolved" || fail "git did not resolve $url_b to its mirror: $resolved"

refuses() { # NAME PATTERN
  grep -qx 'rc=1' <<<"$out" || fail "$1: not refused: $out"
  if grep -q '^GIT_CONFIG_COUNT=' <<<"$out"; then fail "$1: exported GIT_CONFIG_COUNT: $out"; fi
  grep -qF -- "$2" <<<"$out" || fail "$1: message lacks '$2': $out"
}

# Refuse: a missing mirror.
mkdir -p "$tmp/only-a"
git clone -q --mirror "$tmp/up/a" "$tmp/only-a/a.git"
lockfile=$tmp/Cargo.lock dir=$tmp/only-a
out=$(apply)
refuses "missing mirror" "git clone --mirror $url_b $tmp/only-a/b.git"

# Refuse: a mirror without one pinned commit.
mkdir -p "$tmp/stale"
git clone -q --mirror "$tmp/up/a" "$tmp/stale/a.git"
git clone -q --mirror "$tmp/up/b" "$tmp/stale/b.git"
git -C "$tmp/stale/b.git" update-ref refs/heads/main "$rev_b1"
git -C "$tmp/stale/b.git" -c gc.reflogExpire=now reflog expire --all
git -C "$tmp/stale/b.git" gc -q --prune=now
git -C "$tmp/stale/b.git" cat-file -e "$rev_b2^{commit}" 2>/dev/null &&
  { echo "test-git-dep-mirrors: fixture still holds $rev_b2" >&2; exit 2; }
lockfile=$tmp/Cargo.lock dir=$tmp/stale
out=$(apply)
refuses "stale mirror" "$tmp/stale/b.git lacks $rev_b2"
if grep -qF "lacks $rev_b1" <<<"$out"; then fail "stale mirror: refused a commit it holds: $out"; fi

# Refuse: two sources whose last component names the same mirror.
sed "s|$url_b|https://example.invalid/other/a|" "$tmp/Cargo.lock" >"$tmp/collide.lock"
git -C "$mirrors/a.git" fetch -q "$tmp/up/b" "+refs/heads/main:refs/heads/b"
lockfile=$tmp/collide.lock dir=$mirrors
out=$(apply)
refuses "shared mirror" "would share the mirror $mirrors/a.git"

# Refuse: no git sources, and no mirror directory.
grep -v '^source = "git+' "$tmp/Cargo.lock" >"$tmp/registry-only.lock"
lockfile=$tmp/registry-only.lock dir=$mirrors
out=$(apply)
refuses "no git sources" "has no git sources"
lockfile=$tmp/Cargo.lock dir=$tmp/absent
out=$(apply)
refuses "absent directory" "is not a directory"

# Coverage: every git source of the real lockfile is named.
repo_lock=$script_dir/../Cargo.lock
mkdir -p "$tmp/empty"
lockfile=$repo_lock dir=$tmp/empty
out=$(apply)
grep -qx 'rc=1' <<<"$out" || fail "real lockfile with no mirrors was not refused: $out"
sources=$(grep -o '^source = "git+[^?#"]*' "$repo_lock" | sed 's/^source = "git+//' | sort -u)
[[ -n $sources ]] || fail "the repository's Cargo.lock has no git sources to check"
while read -r url; do
  grep -qF "no mirror of $url:" <<<"$out" || fail "real lockfile: $url not named: $out"
done <<<"$sources"

if [[ $failures -ne 0 ]]; then
  echo "test-git-dep-mirrors: $failures failure(s)" >&2
  exit 1
fi
echo "test-git-dep-mirrors: ok ($(wc -l <<<"$sources") git sources in Cargo.lock covered)"
