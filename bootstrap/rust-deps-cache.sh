# shellcheck shell=bash
# Sourced by bootstrap/regenerate-rust-deps. Keeps what Reindeer generates (the
# vendored crates, the raw BUCK before postprocessing, and the vendored-sources
# Cargo configuration) in a cache entry named by everything Reindeer reads, so a
# checkout whose inputs match an earlier run's restores them instead of
# downloading and vendoring every crate again. A validation checkout is new for
# every run, so only a cache outside the checkout can serve it.
#
# The key is computed by bootstrap/rust-deps-cache-key.rs, which lists what it
# covers: every input Reindeer reads, parsed with real TOML and JSON parsers.
# Settings that make Reindeer read files or a compiler the key does not cover
# decline the cache, and an input the helper cannot read is an error; either
# way the run generates and stores nothing.
# The postprocessing (patch-reverie-dbt-buck.rs, the shared-cell aliases) reads
# the reverie submodule, so it is not cached: it runs on the restored BUCK as on
# a generated one.
#
# The root is resolved as bootstrap/run-pinned-tool resolves its cache, with
# rust-deps/ beneath it, so a validation service's HERMIT_BUCK2_SHARED_TOOL_CACHE
# serves every run on the host. HERMIT_RUST_DEPS_CACHE=off disables it. Like the
# tool cache, it trusts every checkout that writes to it: a hit is not
# re-verified, but an entry a restore cannot copy is discarded and generated
# again. An entry is public under its key only once complete (built in a staging
# directory, renamed in) and leaves that name before anything in it is removed
# (renamed out to an .evict- directory), so an interrupted publish or eviction
# never leaves a partial entry a restore accepts. A publisher holds a lock on
# its staging directory from creating it until it is renamed in, and only
# staging nobody holds is removed, so a live publish's staging is never touched
# by another run. Entries are about 400 MB; the
# newest HERMIT_RUST_DEPS_CACHE_KEEP (default 3) are kept, the rest are removed
# when an entry is published.
#
# These functions run where the caller's `set -e` is suspended (in conditions
# and `||` lists), so every step checks its own status.

# rust_deps_cache_root prints the cache directory, or nothing when it is disabled.
rust_deps_cache_root() {
  [[ ${HERMIT_RUST_DEPS_CACHE:-} != off ]] || return 0
  local base
  if [[ -n ${HERMIT_BUCK2_SHARED_TOOL_CACHE:-} ]]; then
    base=$HERMIT_BUCK2_SHARED_TOOL_CACHE
  elif [[ -n ${HERMIT_BUCK2_TOOL_CACHE:-} ]]; then
    base=$HERMIT_BUCK2_TOOL_CACHE
  elif [[ -n ${XDG_CACHE_HOME:-} ]]; then
    base=$XDG_CACHE_HOME/hermit-buck2-tools
  else
    base=$HOME/.cache/hermit-buck2-tools
  fi
  echo "$base/rust-deps"
}

# rust_deps_cache_key REPO_ROOT prints the key of the checkout at REPO_ROOT and
# succeeds. When the cache cannot serve that checkout it prints why and returns
# 3; when the key could not be computed it prints nothing and returns 1, with
# the reason on standard error.
rust_deps_cache_key() {
  local out status=0
  out=$("$1/bootstrap/rust-deps-cache-key.rs" "$1") || status=$?
  case $status in
    0)
      [[ $out =~ ^[0-9a-f]{64}$ ]] || {
        echo "rust-deps cache: the key helper printed '$out', not a key" >&2
        return 1
      }
      ;;
    3) [[ -n $out ]] || out="the key helper declined without giving a reason" ;;
    *)
      echo "rust-deps cache: the key helper failed (exit $status)" >&2
      return 1
      ;;
  esac
  echo "$out"
  return "$status"
}

# rust_deps_cache_restore ROOT KEY THIRD_PARTY_DIR RAW_BUCK copies a cached entry
# into THIRD_PARTY_DIR (vendor/, .cargo/config.toml) and its raw BUCK to RAW_BUCK.
# Returns 0 on a hit, 1 when there is no entry (nothing copied), 2 when a copy
# failed (the output may be partial).
rust_deps_cache_restore() {
  local root=$1 key=$2 third_party_dir=$3 raw_buck=$4
  local entry=$root/$key
  [[ -d $root ]] || return 1
  (
    # Shared: a publish's eviction waits until no restore is copying.
    exec 9>>"$root/.lock" || exit 2
    flock -s 9 || exit 2
    [[ -f $entry/BUCK.reindeer ]] || exit 1
    touch -- "$entry" || exit 2
    mkdir -p -- "$third_party_dir/.cargo" || exit 2
    cp -a --reflink=auto -- "$entry/vendor" "$third_party_dir/vendor" || exit 2
    cp -- "$entry/cargo-config.toml" "$third_party_dir/.cargo/config.toml" || exit 2
    cp -- "$entry/BUCK.reindeer" "$raw_buck" || exit 2
  )
}

# rust_deps_cache_retire ROOT NAME, with the exclusive lock held, renames the
# entry ROOT/NAME out of the key namespace, then removes it. A failed removal
# leaves an .evict- directory no restore reads, removed by a later publish.
rust_deps_cache_retire() {
  local root=$1 name=$2 grave
  [[ -e $root/$name || -L $root/$name ]] || return 0
  grave=$(mktemp -d "$root/.evict-$name-XXXXXX") || return 1
  if ! mv -T -- "$root/$name" "$grave/entry"; then
    rmdir -- "$grave"
    return 1
  fi
  rm -rf -- "${grave:?}" || {
    echo "warning: could not remove the retired entry $grave; the next publish retries" >&2
    return 0
  }
}

# rust_deps_cache_discard ROOT KEY removes an entry a restore could not copy, so
# this run's publish replaces it. A failure leaves it to fail, and be
# regenerated around, again.
rust_deps_cache_discard() {
  local root=$1 key=$2
  (
    exec 9>>"$root/.lock" || exit 1
    flock 9 || exit 1
    rust_deps_cache_retire "$root" "$key"
  ) || echo "warning: could not remove $root/$key" >&2
  return 0
}

# rust_deps_cache_publish ROOT KEY THIRD_PARTY_DIR RAW_BUCK stores a generated
# entry, then removes all but the newest HERMIT_RUST_DEPS_CACHE_KEEP. A failure
# here costs later runs their hit, never this run its output, so it warns and
# returns 0.
rust_deps_cache_publish() {
  local root=$1 key=$2 third_party_dir=$3 raw_buck=$4
  local keep=${HERMIT_RUST_DEPS_CACHE_KEEP:-3}
  if [[ ! $keep =~ ^[1-9][0-9]*$ ]]; then
    echo "warning: HERMIT_RUST_DEPS_CACHE_KEEP=$keep is not a positive integer; not caching" >&2
    return 0
  fi
  (
    mkdir -p -- "$root" || exit 1
    exec 9>>"$root/.lock" || exit 1
    # The staging directory is created and locked under the exclusive lock,
    # which the sweep below holds, so the sweep never finds it unlocked while
    # this run lives. The copy runs without the exclusive lock.
    flock 9 || exit 1
    staging=$(mktemp -d "$root/.staging-$key-XXXXXX") || exit 1
    trap '[[ -z $staging ]] || rm -rf -- "${staging:?}"' EXIT
    exec 8<"$staging" || exit 1
    flock -n 8 || exit 1
    flock -u 9 || exit 1
    cp -a --reflink=auto -- "$third_party_dir/vendor" "$staging/vendor" || exit 1
    cp -- "$third_party_dir/.cargo/config.toml" "$staging/cargo-config.toml" || exit 1
    # Written last: restore treats an entry without it as absent.
    cp -- "$raw_buck" "$staging/BUCK.reindeer" || exit 1
    # mktemp creates both private; the entry is shared like the tool cache.
    chmod 0644 -- "$staging/BUCK.reindeer" || exit 1
    chmod 0755 -- "$staging" || exit 1
    flock 9 || exit 1
    # A complete entry another run published stays; anything else under the
    # key is replaced.
    if [[ ! -f $root/$key/BUCK.reindeer ]]; then
      rust_deps_cache_retire "$root" "$key" || exit 1
      mv -T -- "$staging" "$root/$key" || exit 1
      staging=
    fi
    # Newest first by modification time; restore touches the entry it uses.
    old=$(find "$root" -mindepth 1 -maxdepth 1 -type d -regextype posix-extended \
      -regex '.*/[0-9a-f]{64}' -printf '%T@ %f\n' | sort -rn | tail -n +"$((keep + 1))" |
      cut -d' ' -f2-) || exit 1
    status=0
    while IFS= read -r name; do
      [[ -n $name ]] || continue
      rust_deps_cache_retire "$root" "$name" || { echo "warning: could not evict $root/$name" >&2; status=1; }
    done <<<"$old"
    # What an interrupted eviction left (every eviction runs under the
    # exclusive lock, so none is in progress), and staging an interrupted
    # publish left: staging whose lock nobody holds. Neither is under a key.
    find "$root" -mindepth 1 -maxdepth 1 -type d -name '.evict-*' -exec rm -rf -- {} + ||
      { echo "warning: could not remove every retired entry in $root" >&2; status=1; }
    for abandoned in "$root"/.staging-*; do
      [[ -d $abandoned && ! -L $abandoned && $abandoned != "$staging" ]] || continue
      flock -n "$abandoned" true || continue
      rust_deps_cache_retire "$root" "${abandoned##*/}" ||
        { echo "warning: could not remove abandoned staging $abandoned" >&2; status=1; }
    done
    exit "$status"
  ) || echo "warning: storing the generated dependencies in $root did not complete (above); later runs may vendor again" >&2
  return 0
}
