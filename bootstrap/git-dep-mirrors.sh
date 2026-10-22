# shellcheck shell=bash
# Sourced by bootstrap/regenerate-rust-deps. Serves Cargo.lock's git dependencies
# from local mirrors, for hosts whose proxy refuses github.com to the build while
# allowing crates.io (a Meta devserver agent, for one).
#
# HERMIT_GIT_DEP_MIRRORS=DIR names a directory holding a bare mirror per git source,
# named after the URL's last component: https://github.com/rrnewton/liteinst2 is
# served from DIR/liteinst2.git, https://github.com/rrnewton/reverie.git from
# DIR/reverie.git. Two sources with the same last component would share a mirror, and
# are refused. Once it is set, every git source must come from there: a
# missing mirror, or one without a commit Cargo.lock pins, is refused before
# Reindeer runs. Cargo.lock pins every git source to a commit, so a mirror
# cannot change what is built, only where the objects come from.
#
# git_dep_mirrors_apply LOCKFILE DIR exports one url.<mirror>.insteadOf entry per
# source through GIT_CONFIG_COUNT (appended to any the caller already set), and
# CARGO_NET_GIT_FETCH_WITH_CLI=true: Cargo's built-in git client does not read
# GIT_CONFIG_* entries, the git executable does.

git_dep_mirrors_apply() {
  local lockfile=$1 dir=$2 url rev name mirror prev='' missing=0 count=${GIT_CONFIG_COUNT:-0} served=0
  local -A source_of
  [[ -d $dir ]] || { echo "HERMIT_GIT_DEP_MIRRORS=$dir is not a directory" >&2; return 1; }
  dir=$(cd -- "$dir" && pwd)
  # One "URL COMMIT" line per locked git package, sorted so each URL's lines are adjacent.
  local pairs
  pairs=$(sed -n 's/^source = "git+\([^?#"]*\)[^#"]*#\([0-9a-f]\{40\}\)"$/\1 \2/p' "$lockfile" | LC_ALL=C sort -u)
  [[ -n $pairs ]] || { echo "HERMIT_GIT_DEP_MIRRORS is set but $lockfile has no git sources" >&2; return 1; }
  while read -r url rev; do
    name=${url##*/}
    mirror=$dir/${name%.git}.git
    if [[ $url != "$prev" ]]; then
      prev=$url
      if [[ -n ${source_of[$mirror]:-} ]]; then
        echo "$url and ${source_of[$mirror]} would share the mirror $mirror" >&2
        missing=1
        continue
      fi
      source_of[$mirror]=$url
      if [[ ! -d $mirror ]]; then
        echo "no mirror of $url: create it with  git clone --mirror $url $mirror" >&2
        missing=1
        continue
      fi
      export "GIT_CONFIG_KEY_$count=url.$mirror.insteadOf" "GIT_CONFIG_VALUE_$count=$url"
      count=$((count + 1)) served=$((served + 1))
    fi
    [[ -d $mirror ]] || continue
    git -C "$mirror" cat-file -e "$rev^{commit}" 2>/dev/null || {
      echo "$mirror lacks $rev, which Cargo.lock pins for $url: update it with  git -C $mirror fetch" >&2
      missing=1
    }
  done <<<"$pairs"
  [[ $missing -eq 0 ]] || return 1
  export GIT_CONFIG_COUNT=$count CARGO_NET_GIT_FETCH_WITH_CLI=true
  echo "serving $served git dependencies from $dir" >&2
}
