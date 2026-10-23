#!/usr/bin/env bash
# Regression test for the cache in bootstrap/regenerate-rust-deps
# (bootstrap/rust-deps-cache.sh).
#
# Runs the real regenerate-rust-deps, cache functions and key helper
# (rust-deps-cache-key.rs, run from this checkout against the fixture) in a
# fixture checkout whose cargo, Reindeer, feature check and reverie-dbt patch
# are stubs; the
# Reindeer stub logs each call and derives its output from Cargo.lock. Accept:
# the first run vendors and stores an entry; a second run with the same inputs
# calls Reindeer not at all and leaves a byte-identical tree, with the
# postprocessing applied once. Re-vendor: a committed Cargo.lock that moves a
# git dependency to another commit, an edited reindeer.toml, a new untracked
# fixup, and a different cargo version each vendor again, and a lock moved back
# restores its earlier entry. Off: HERMIT_RUST_DEPS_CACHE=off always vendors
# and stores nothing. Eviction keeps the newest HERMIT_RUST_DEPS_CACHE_KEEP
# entries by last use. Damage: an entry whose vendored crates are gone is
# discarded, the run vendors, and the replacement entry serves the next run.
#
# Stale and partial entries: a run whose Reindeer rewrites Cargo.lock fails and
# stores nothing, so the original lock vendors again; a keyed input that changes
# while Reindeer runs stores nothing; a fixup Git ignores, a fixup reached
# through a directory link (its referent edited), a workspace member manifest
# Git ignores (also at a path JSON must escape), a Buck cell marker added
# nearer the third-party directory, a .gitignore outside the checkout that the
# nearest marker brings into force, two checkouts whose paths below the marker
# (so Reindeer's package) differ, a local package under a third-party
# directory named target, git, registry or .package-cache, and the compiler's
# target cfg and version each change the key, and RUSTC names the compiler
# queried; a
# platform target written as a multi-line or escaped string, or under an escaped
# key, is queried as Reindeer decodes it; reindeer.toml settings the key cannot
# cover (fixups_dir, cargo.rustc, cargo.cargo, gitignore_checksum_exclude, an
# unknown key, no platform table), also under escaped keys, vendor without
# storing, while a multi-line string elsewhere and Hermit's own reindeer.toml
# keep the cache; a key that cannot be computed (cargo metadata failing or
# printing broken JSON, an unreadable directory under the third-party
# directory, a helper printing something other than a key or declining without
# a reason), before or after Reindeer runs, vendors without
# restoring or storing; an eviction whose removal fails part-way leaves no
# entry a restore accepts; an entry without its raw BUCK is not copied from and
# is replaced by the next publish; and a publish paused with its staging complete keeps that staging,
# however old, while another run publishes and removes abandoned staging, then
# stores an entry that restores.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
for f in regenerate-rust-deps rust-deps-cache.sh rust-deps-cache-key.rs; do
  [[ -f $script_dir/$f ]] || { echo "test-rust-deps-cache: missing $script_dir/$f" >&2; exit 2; }
done

# The fixture must have no Buck cell marker (.buckconfig, .buckroot) above it
# (see "Buck cell markers" below). A TMPDIR inside a Buck checkout would put
# the checkout's own marker above it: validate points TMPDIR into the
# checkout's target/validation when the caller sets none, as on a GitHub-hosted
# runner, and hermit tracks a .buckconfig at its root. So the fixture takes the
# first temporary root with no marker above it.
marker_free() {
  local up
  up=$(cd -- "$1" 2>/dev/null && pwd -P) || return 1
  while :; do
    [[ ! -e $up/.buckconfig && ! -e $up/.buckroot ]] || return 1
    [[ $up != / ]] || return 0
    up=$(dirname -- "$up")
  done
}
tmp_root=
for candidate in "${TMPDIR:-/tmp}" /tmp /var/tmp; do
  if marker_free "$candidate"; then
    tmp_root=$candidate
    break
  fi
done
[[ -n $tmp_root ]] || {
  echo "test-rust-deps-cache: TMPDIR, /tmp and /var/tmp each have a Buck cell marker above them" >&2
  exit 2
}
tmp=$(mktemp -d -p "$tmp_root")
trap 'rm -rf "${tmp:?}"' EXIT
failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }

fixture_git() {
  git -c user.email=fixture@example.invalid -c user.name=fixture \
    -c commit.gpgsign=false -c init.defaultBranch=main "$@"
}

repo=$tmp/repo
cache=$tmp/cache
mkdir -p "$tmp/bin" "$repo/bootstrap" "$repo/scripts" "$repo/reverie" \
  "$repo/shim/third-party/rust/fixups/one"
cp -- "$script_dir/regenerate-rust-deps" "$script_dir/rust-deps-cache.sh" "$repo/bootstrap/"

# The key helper is the real one, built and run from this checkout (where its
# toolchain and any prepared binary resolve), against the fixture. rust-script
# builds it with the real cargo, which the stub passes anything but --version
# and metadata to.
cat >"$repo/bootstrap/rust-deps-cache-key.rs" <<EOF
#!/usr/bin/env bash
exec env -C "$script_dir/.." "$script_dir/rust-deps-cache-key.rs" "\$@"
EOF
real_cargo=$(type -P cargo || true)
cat >"$tmp/bin/cargo" <<EOF
#!/usr/bin/env bash
real_cargo='$real_cargo'
real_path='$PATH'
EOF
cat >>"$tmp/bin/cargo" <<'EOF'
case $1 in
  --version) echo "${FIXTURE_CARGO_VERSION:-cargo 1.0.0 (fixture)}" ;;
  # Each workspace member the fixture has, as `cargo metadata` names it: in
  # JSON, so a backslash or a quote in its path is escaped.
  metadata)
    # FIXTURE_METADATA_FAIL=FILE: every call after the first (regenerate-rust-deps'
    # own check) fails.
    if [[ -n ${FIXTURE_METADATA_FAIL:-} ]]; then
      echo call >>"$FIXTURE_METADATA_FAIL"
      [[ $(wc -l <"$FIXTURE_METADATA_FAIL") -lt 2 ]] || { echo "fixture cargo: metadata failed" >&2; exit 101; }
    fi
    [[ -z ${FIXTURE_METADATA_JSON:-} ]] || { echo "$FIXTURE_METADATA_JSON"; exit 0; }
    packages=
    for manifest in member*/Cargo.toml; do
      [[ -f $manifest ]] || continue
      path=$PWD/$manifest
      path=${path//\\/\\\\}
      path=${path//\"/\\\"}
      packages+="${packages:+,}{\"name\":\"member\",\"manifest_path\":\"$path\"}"
    done
    echo "{\"packages\":[$packages]}"
    ;;
  *)
    [[ -n $real_cargo ]] || { echo "fixture cargo: no real cargo for $*" >&2; exit 2; }
    exec env -u RUSTC PATH="$real_path" "$real_cargo" "$@"
    ;;
esac
EOF
cat >"$tmp/bin/rustc" <<'EOF'
#!/usr/bin/env bash
case $1 in
  -vV) echo "${FIXTURE_RUSTC_VERSION:-rustc 1.0.0 (fixture)}" ;;
  --print=cfg)
    echo 'debug_assertions'
    echo "target_arch=\"${3%%-*}\""
    [[ -z ${FIXTURE_RUSTC_CFG:-} ]] || echo "$FIXTURE_RUSTC_CFG"
    [[ $3 != *-musl || -z ${FIXTURE_RUSTC_MUSL_CFG:-} ]] || echo "$FIXTURE_RUSTC_MUSL_CFG"
    ;;
  *) echo "fixture rustc: unexpected $*" >&2; exit 2 ;;
esac
EOF
# A flock that, when FIXTURE_FLOCK_PAUSE names a cache root holding complete
# staging, pauses before taking the exclusive lock (`flock 9`) until
# $FIXTURE_FLOCK_PAUSE.resume exists: a publish stopped after filling its
# staging and before renaming it in.
mkdir -p "$tmp/flockbin"
cat >"$tmp/flockbin/flock" <<EOF
#!/usr/bin/env bash
if [[ \$* == 9 && -n \${FIXTURE_FLOCK_PAUSE:-} ]] &&
  compgen -G "\$FIXTURE_FLOCK_PAUSE/.staging-*/BUCK.reindeer" >/dev/null; then
  touch -- "\$FIXTURE_FLOCK_PAUSE.paused"
  until [[ -e \$FIXTURE_FLOCK_PAUSE.resume ]]; do sleep 0.1; done
fi
exec "$(type -P flock)" "\$@"
EOF
# A RUSTC wrapper that adds a target feature, as a -C target-feature flag would.
cat >"$tmp/rustc-wrapper" <<EOF
#!/usr/bin/env bash
FIXTURE_RUSTC_CFG='target_feature="avx2"' exec "$tmp/bin/rustc" "\$@"
EOF
# An rm that, given a path in the cache, deletes one vendored file and fails,
# as an interrupted or failing recursive removal leaves an entry.
mkdir -p "$tmp/rmbin"
cat >"$tmp/rmbin/rm" <<EOF
#!/usr/bin/env bash
for arg; do
  case \$arg in
    "$tmp/cache/rust-deps/"*)
      victim=\$(/usr/bin/find "\$arg" -type f -name source -print -quit 2>/dev/null)
      [[ -z \$victim ]] || "$(type -P rm)" -f -- "\$victim"
      echo "fixture rm: failed part-way through \$arg" >&2
      exit 1
      ;;
  esac
done
exec "$(type -P rm)" "\$@"
EOF
# Reindeer stub: --third-party-dir DIR --manifest-path PATH vendor|buckify [--stdout].
# Its BUCK names the lock, the fixups it reads through links, the target cfg
# the compiler reports, and, as pinned Reindeer does, the Buck package below
# the nearest .buckconfig or .buckroot with the .gitignore files from there
# down (src/path.rs, src/gitignore.rs), and every file in the third-party
# directory but the vendor, .cargo and BUCK it writes. FIXTURE_REWRITE_LOCK=REV makes vendor resolve the lock
# to REV, as pinned Reindeer may (it vendors with locked: false);
# FIXTURE_EDIT_FIXUP=1 makes it edit a fixup while it runs.
cat >"$repo/bootstrap/reindeer" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
dir=$2
echo "$5" >>"$FIXTURE_REINDEER_LOG"
if [[ $5 == vendor && -n ${FIXTURE_REWRITE_LOCK:-} ]]; then
  sed -i -E "s/rev=[0-9a-f]{40}#[0-9a-f]{40}/rev=$FIXTURE_REWRITE_LOCK#$FIXTURE_REWRITE_LOCK/" Cargo.lock
fi
if [[ $5 == vendor && -n ${FIXTURE_EDIT_FIXUP:-} ]]; then
  echo '# edited while vendoring' >>"$dir/fixups/one/fixups.toml"
fi
lock=$(sha256sum <Cargo.lock | cut -c1-64)
fixups=$(find -L "$dir/fixups" -type f -print0 | LC_ALL=C sort -z | xargs -0 cat | sha256sum | cut -c1-64)
cfg=$("${RUSTC-rustc}" --print=cfg --target x86_64-unknown-linux-gnu | grep '^target_' | sha256sum | cut -c1-64)
real=$(cd -- "$dir" && pwd -P)
chain=() up=$real package=none
while :; do
  chain+=("$up/.gitignore")
  if [[ -e $up/.buckconfig || -e $up/.buckroot ]]; then package=${real#"$up"}; break; fi
  if [[ $up == / ]]; then chain=("$real/.gitignore"); break; fi
  up=$(dirname -- "$up")
done
ignores=$({ cat -- "${chain[@]}" 2>/dev/null || true; } | sha256sum | cut -c1-64)
files=$( (cd -- "$dir" && { find -L . \( -path ./vendor -o -path ./.cargo -o -path ./BUCK \) -prune \
  -o -type f -print0 2>/dev/null || true; } | LC_ALL=C sort -z | { xargs -0 -r sha256sum 2>/dev/null || true; }) |
  sha256sum | cut -c1-64)
case $5 in
  vendor)
    mkdir -p "$dir/vendor/crate" "$dir/.cargo/registry/cache"
    echo "$lock" >"$dir/vendor/crate/source"
    echo download >"$dir/.cargo/registry/cache/crate.crate"
    printf '[source.vendored-sources]\ndirectory = "vendor"\n' >"$dir/.cargo/config.toml"
    ;;
  buckify)
    raw="# raw BUCK for $lock; fixups $fixups; cfg $cfg; package $package; ignores $ignores; files $files"
    if [[ ${6:-} == --stdout ]]; then echo "$raw"
    else echo "$raw" >"$dir/BUCK"; fi
    ;;
esac
EOF
printf '#!/usr/bin/env bash\n' >"$repo/scripts/check-buck-reindeer-features.rs"
cat >"$repo/scripts/patch-reverie-dbt-buck.rs" <<'EOF'
#!/usr/bin/env bash
echo "# patched" >>"$1"
EOF
chmod +x "$tmp/bin/cargo" "$tmp/bin/rustc" "$tmp/rustc-wrapper" "$tmp/rmbin/rm" "$tmp/flockbin/flock" \
  "$repo/bootstrap/reindeer" "$repo/bootstrap/rust-deps-cache-key.rs" "$repo/scripts/"*.rs
echo '# none' >"$repo/shim/third-party/rust/shared-cell-aliases.txt"
reindeer_toml=$repo/shim/third-party/rust/reindeer.toml
printf '[cargo]\n\n[platform.linux-x86_64]\ntarget = "x86_64-unknown-linux-gnu"\n' >"$reindeer_toml"
echo 'extra_srcs = []' >"$repo/shim/third-party/rust/fixups/one/fixups.toml"
echo 'buck' >"$repo/reverie/BUCK"
printf '[workspace]\nmembers = []\n' >"$repo/Cargo.toml"
printf '/shim/third-party/rust/.cargo/\n/shim/third-party/rust/BUCK\n/shim/third-party/rust/vendor/\n' \
  >"$repo/.gitignore"
lock_at() {
  cat >"$repo/Cargo.lock" <<EOF
version = 4

[[package]]
name = "dep"
version = "0.1.0"
source = "git+https://example.invalid/org/dep?rev=$1#$1"
EOF
}
commit_all() {
  fixture_git -C "$repo" add -A
  fixture_git -C "$repo" commit -qm "$1"
}
rev_a=1111111111111111111111111111111111111111
rev_b=2222222222222222222222222222222222222222
fixture_git init -q "$repo"
lock_at "$rev_a"
commit_all base

# What the key helper's build needs from the caller's environment: the
# toolchain, rust-script's cache, and the prepared-binary runner's settings.
toolchain_env=(
  RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
  CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
  XDG_CACHE_HOME="${XDG_CACHE_HOME:-$HOME/.cache}"
)
for var in RUSTUP_TOOLCHAIN HERMIT_RUST_SCRIPT_ARTIFACT_ROOT HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED \
  HERMIT_REAL_RUST_SCRIPT; do
  [[ -z ${!var:-} ]] || toolchain_env+=("$var=${!var}")
done

# regen NAME [ENV...]: runs regenerate-rust-deps in $regen_repo (default the
# fixture) with a private cache root that keeps every entry unless ENV says
# otherwise; records its rc, stderr, the Reindeer calls it made, and a hash of
# the tree.
regen_repo=$repo
regen() {
  local name=$1
  shift
  local log=$tmp/$name.reindeer.log
  : >"$log"
  local rc=0
  env -i PATH="$tmp/bin:$PATH" HOME="$tmp" GIT_CONFIG_NOSYSTEM=1 "${toolchain_env[@]}" \
    HERMIT_BUCK2_SHARED_TOOL_CACHE="$cache" HERMIT_RUST_DEPS_CACHE_KEEP=100 \
    FIXTURE_REINDEER_LOG="$log" "$@" \
    "$regen_repo/bootstrap/regenerate-rust-deps" >"$tmp/$name.out" 2>"$tmp/$name.err" || rc=$?
  echo "$rc" >"$tmp/$name.rc"
  tr '\n' ' ' <"$log" >"$tmp/$name.calls"
  (cd "$regen_repo/shim/third-party/rust" && find BUCK .cargo vendor -type f -print0 |
    LC_ALL=C sort -z | xargs -0 sha256sum) >"$tmp/$name.tree" 2>&1 || true
}
calls() { cat "$tmp/$1.calls"; }
# entries [CACHE] counts the entries under a key in CACHE (default the shared one).
entries() {
  local root=${1:-$cache}/rust-deps
  [[ -d $root ]] || { echo 0; return; }
  find "$root" -mindepth 1 -maxdepth 1 -type d -regextype posix-extended \
    -regex '.*/[0-9a-f]{64}' | wc -l
}
# stored counts everything under the shared cache root but its lock and the
# staging and retired directories, so an entry under a name no key has counts.
stored() {
  local root=$cache/rust-deps
  [[ -d $root ]] || { echo 0; return; }
  find "$root" -mindepth 1 -maxdepth 1 ! -name '.*' | wc -l
}
expect_rc() {
  [[ $(cat "$tmp/$1.rc") == "$2" ]] || fail "$1: expected exit $2, got $(cat "$tmp/$1.rc"): $(cat "$tmp/$1.err")"
}
expect_ok() {
  [[ $(cat "$tmp/$1.rc") == 0 ]] || { fail "$1: exit $(cat "$tmp/$1.rc"): $(cat "$tmp/$1.err")"; return 0; }
}
expect_vendored() {
  expect_ok "$1"
  [[ $(calls "$1") == "vendor buckify buckify " ]] || fail "$1: expected Reindeer to vendor, called: '$(calls "$1")'"
  ! grep -q '^restored' "$tmp/$1.err" || fail "$1: reported a restore"
}
expect_restored() {
  expect_ok "$1"
  [[ -z $(calls "$1") ]] || fail "$1: expected a restore, but Reindeer ran: '$(calls "$1")'"
  grep -q '^restored the vendored crates' "$tmp/$1.err" || fail "$1: no restore reported"
}
buck=$repo/shim/third-party/rust/BUCK

# First run vendors, prunes Cargo's download caches, and stores one entry.
regen first
expect_vendored first
[[ $(entries) == 1 ]] || fail "first: expected 1 cache entry, found $(entries)"
[[ ! -e $repo/shim/third-party/rust/.cargo/registry ]] || fail "first: Cargo's download cache was kept"
[[ $(grep -c '^# patched$' "$buck") == 1 ]] || fail "first: postprocessing ran $(grep -c '^# patched$' "$buck") times"

# Same inputs: no Reindeer call, the same tree, postprocessing applied once.
regen second
expect_restored second
cmp -s "$tmp/first.tree" "$tmp/second.tree" || fail "second: restored tree differs from the generated one"
[[ $(entries) == 1 ]] || fail "second: expected 1 cache entry, found $(entries)"

# A committed Cargo.lock that moves a git dependency vendors again, for its lock.
lock_at "$rev_b"
commit_all "move dep"
regen lock_b
expect_vendored lock_b
cmp -s "$tmp/first.tree" "$tmp/lock_b.tree" && fail "lock_b: tree did not change with the lock"
grep -q "$(sha256sum <"$repo/Cargo.lock" | cut -c1-64)" "$buck" || fail "lock_b: BUCK is not the new lock's"
[[ $(entries) == 2 ]] || fail "lock_b: expected 2 cache entries, found $(entries)"

# Moving the lock back restores the first entry.
lock_at "$rev_a"
commit_all "move dep back"
regen lock_a
expect_restored lock_a
cmp -s "$tmp/first.tree" "$tmp/lock_a.tree" || fail "lock_a: tree differs from the first run's"

# An uncommitted reindeer.toml edit, a new untracked fixup, and another cargo
# version each change the key.
echo 'vendor = true' >>"$repo/shim/third-party/rust/reindeer.toml"
regen reindeer_toml
expect_vendored reindeer_toml
fixture_git -C "$repo" checkout -q -- shim/third-party/rust/reindeer.toml
mkdir -p "$repo/shim/third-party/rust/fixups/two"
echo 'extra_srcs = []' >"$repo/shim/third-party/rust/fixups/two/fixups.toml"
regen new_fixup
expect_vendored new_fixup
rm -rf -- "${repo:?}/shim/third-party/rust/fixups/two"
regen cargo_version FIXTURE_CARGO_VERSION="cargo 1.0.1 (fixture)"
expect_vendored cargo_version

# Off: vendors although an entry matches, and stores nothing.
before=$(entries)
regen off HERMIT_RUST_DEPS_CACHE=off
expect_vendored off
cmp -s "$tmp/first.tree" "$tmp/off.tree" || fail "off: tree differs from the first run's"
[[ $(entries) == "$before" ]] || fail "off: cache changed from $before to $(entries) entries"

# Eviction by last use: of the five entries, use the oldest, then publish a
# sixth with KEEP=2; those two remain.
[[ $(entries) == 5 ]] || fail "before eviction: expected 5 cache entries, found $(entries)"
regen use_base
expect_restored use_base
base_key=$(sed -n 's/^restored .* from .*\/\([0-9a-f]\{64\}\)$/\1/p' "$tmp/use_base.err")
regen keep2 HERMIT_RUST_DEPS_CACHE_KEEP=2 FIXTURE_CARGO_VERSION="cargo 1.0.2 (fixture)"
expect_vendored keep2
[[ $(entries) == 2 ]] || fail "keep2: expected 2 cache entries, found $(entries)"
[[ -n $base_key && -d $cache/rust-deps/$base_key ]] || fail "keep2: evicted the most recently used entry"

# Damage: an entry without its vendored crates is discarded and replaced.
rm -rf -- "${cache:?}/rust-deps/${base_key:?}/vendor"
regen damaged
expect_vendored damaged
grep -q 'discarding it and generating' "$tmp/damaged.err" || fail "damaged: no discard reported"
cmp -s "$tmp/first.tree" "$tmp/damaged.tree" || fail "damaged: tree differs from the first run's"
regen repaired
expect_restored repaired
lock_a_hash=$(sha256sum <"$repo/Cargo.lock" | cut -c1-64)

# A Reindeer that rewrites Cargo.lock fails the run and stores nothing, so the
# original lock vendors again rather than restoring the rewritten lock's output.
regen lock_rewrite HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-lock" FIXTURE_REWRITE_LOCK="$rev_b"
expect_rc lock_rewrite 1
[[ $(entries "$tmp/cache-lock") == 0 ]] || fail "lock_rewrite: stored $(entries "$tmp/cache-lock") entries"
fixture_git -C "$repo" checkout -q -- Cargo.lock
regen lock_retry HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-lock"
expect_vendored lock_retry
grep -q "raw BUCK for $lock_a_hash" "$buck" || fail "lock_retry: BUCK is not the original lock's"

# A keyed input that changes while Reindeer runs: the output is kept, not stored.
regen input_moved HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-moved" FIXTURE_EDIT_FIXUP=1
expect_vendored input_moved
grep -q 'inputs changed while Reindeer ran' "$tmp/input_moved.err" || fail "input_moved: no warning"
[[ $(entries "$tmp/cache-moved") == 0 ]] || fail "input_moved: stored $(entries "$tmp/cache-moved") entries"
fixture_git -C "$repo" checkout -q -- shim/third-party/rust/fixups/one/fixups.toml

# A new fixup Git ignores through .git/info/exclude; Reindeer reads it anyway.
fixups=$repo/shim/third-party/rust/fixups
echo '/shim/third-party/rust/fixups/excluded/' >>"$repo/.git/info/exclude"
mkdir -p "$fixups/excluded"
echo 'cfgs = ["one"]' >"$fixups/excluded/fixups.toml"
[[ -z $(fixture_git -C "$repo" ls-files --others --exclude-standard -- shim/third-party/rust/fixups/excluded) ]] ||
  fail "excluded_fixup: the fixture's fixup is not excluded"
regen excluded_fixup
expect_vendored excluded_fixup
regen excluded_again
expect_restored excluded_again
echo 'cfgs = ["two"]' >"$fixups/excluded/fixups.toml"
regen excluded_edit
expect_vendored excluded_edit
rm -rf -- "${fixups:?}/excluded"

# A fixup reached through a directory link, then its referent edited.
mkdir -p "$tmp/outside-fixup"
echo 'cfgs = ["one"]' >"$tmp/outside-fixup/fixups.toml"
ln -s -- "$tmp/outside-fixup" "$fixups/linked"
regen link_new
expect_vendored link_new
regen link_again
expect_restored link_again
echo 'cfgs = ["two"]' >"$tmp/outside-fixup/fixups.toml"
regen link_edit
expect_vendored link_edit
rm -f -- "$fixups/linked"

# A workspace member whose manifest Git ignores, then edited.
echo '/member/' >>"$repo/.git/info/exclude"
mkdir -p "$repo/member"
printf '[package]\nname = "member"\n' >"$repo/member/Cargo.toml"
regen member_new
expect_vendored member_new
regen member_again
expect_restored member_again
echo 'edition = "2021"' >>"$repo/member/Cargo.toml"
regen member_edit
expect_vendored member_edit
rm -rf -- "${repo:?}/member"
# The same at a path JSON escapes: only its decoded path names the file.
escaped_member='member\name'
printf '%s\n' '/member\\name/' >>"$repo/.git/info/exclude"
mkdir -p "$repo/$escaped_member"
printf '[package]\nname = "member"\n' >"$repo/$escaped_member/Cargo.toml"
[[ -z $(fixture_git -C "$repo" ls-files --others --exclude-standard -- "$escaped_member") ]] ||
  fail "member_escaped: the fixture's member is not excluded"
regen member_escaped_new
expect_vendored member_escaped_new
regen member_escaped_again
expect_restored member_escaped_again
echo 'edition = "2021"' >>"$repo/$escaped_member/Cargo.toml"
regen member_escaped_edit
expect_vendored member_escaped_edit
rm -rf -- "${repo:?}/${escaped_member:?}"

# expect_input_keyed WARM AGAIN EDIT: WARM vendored a new input and AGAIN
# restored it; EDIT, after the input changed, must vendor (not restore WARM's
# entry) and produce what a run without the cache produces, which differs from
# WARM's tree.
expect_input_keyed() {
  expect_vendored "$1"
  expect_restored "$2"
  expect_vendored "$3"
  regen "$3_off" HERMIT_RUST_DEPS_CACHE=off
  expect_vendored "$3_off"
  cmp -s "$tmp/$3.tree" "$tmp/$3_off.tree" || fail "$3: tree differs from the uncached run's"
  cmp -s "$tmp/$1.tree" "$tmp/$3_off.tree" && fail "$3_off: the change did not change the output"
  return 0
}

# Buck cell markers. The fixture has none up to /, so Reindeer names its
# targets after no package and applies only the third-party .gitignore.
for marker_dir in "$repo/shim/third-party/rust" "$repo/shim/third-party" "$repo/shim" "$repo" "$tmp"; do
  up=$(cd -- "$marker_dir" && pwd -P)
  while :; do
    [[ ! -e $up/.buckconfig && ! -e $up/.buckroot ]] || fail "marker: the fixture has a marker at $up already"
    [[ $up != / ]] || break
    up=$(dirname -- "$up")
  done
done
# A marker nearer the third-party directory than any other: the package
# changes, then the .gitignore it brings into force.
third_party_ignore=$repo/shim/third-party/.gitignore
regen marker_before
touch -- "$repo/shim/third-party/.buckroot"
regen marker_new
expect_vendored marker_new
cmp -s "$tmp/marker_before.tree" "$tmp/marker_new.tree" && fail "marker_new: the marker did not change the output"
regen marker_again
echo '/fixups/one/' >"$third_party_ignore"
regen marker_edit
expect_input_keyed marker_new marker_again marker_edit
rm -f -- "$repo/shim/third-party/.buckroot" "$third_party_ignore"
# A marker outside the checkout: the .gitignore beside it applies too.
outside_ignore=$tmp/.gitignore
echo '/nothing/' >"$outside_ignore"
touch -- "$tmp/.buckroot"
regen outside_new
regen outside_again
echo '/repo/shim/third-party/rust/fixups/one/' >"$outside_ignore"
regen outside_edit
expect_input_keyed outside_new outside_again outside_edit
rm -f -- "$tmp/.buckroot" "$outside_ignore"
# Two clones of one commit under the marker, differing only in their directory
# names: the package, and so every label Reindeer writes, differs between them,
# and nothing else Reindeer reads does.
fixture_git clone -q -- "$repo" "$tmp/package-x"
fixture_git clone -q -- "$repo" "$tmp/package-y"
touch -- "$tmp/.buckroot"
regen_repo=$tmp/package-x
regen package_x_new
regen package_x_again
regen_repo=$tmp/package-y
regen package_y
expect_input_keyed package_x_new package_x_again package_y
regen_repo=$repo
rm -rf -- "${tmp:?}/package-x" "${tmp:?}/package-y"
rm -f -- "$tmp/.buckroot"

# A local package under a third-party directory whose name Cargo also uses for
# output; nothing regenerate-rust-deps runs writes there, so Reindeer reads it.
for output_name in target git registry .package-cache; do
  dep=$repo/shim/third-party/rust/$output_name/dep
  mkdir -p "$dep/src"
  printf '[package]\nname = "dep"\n' >"$dep/Cargo.toml"
  echo 'mod alpha;' >"$dep/src/lib.rs"
  : >"$dep/src/alpha.rs"
  : >"$dep/src/beta.rs"
  case_name=local_${output_name#.}
  regen "${case_name}_new"
  regen "${case_name}_again"
  echo 'mod beta;' >"$dep/src/lib.rs"
  regen "${case_name}_edit"
  expect_input_keyed "${case_name}_new" "${case_name}_again" "${case_name}_edit"
  rm -rf -- "${repo:?}/shim/third-party/rust/${output_name:?}"
done

# The compiler Reindeer queries, with the cargo version unchanged: another
# target cfg, the same again, and another version. A RUSTC wrapper that reports
# the same cfg restores that entry; with RUSTC not followed it would restore the
# plain compiler's.
regen rustc_cfg FIXTURE_RUSTC_CFG='target_feature="avx2"'
expect_vendored rustc_cfg
cmp -s "$tmp/first.tree" "$tmp/rustc_cfg.tree" && fail "rustc_cfg: BUCK did not change with the target cfg"
regen rustc_cfg_again FIXTURE_RUSTC_CFG='target_feature="avx2"'
expect_restored rustc_cfg_again
regen rustc_version FIXTURE_RUSTC_VERSION="rustc 1.0.1 (fixture)"
expect_vendored rustc_version
regen rustc_wrapper RUSTC="$tmp/rustc-wrapper"
expect_restored rustc_wrapper
cmp -s "$tmp/rustc_cfg.tree" "$tmp/rustc_wrapper.tree" || fail "rustc_wrapper: tree differs from the same cfg's"

# A platform target in a multi-line string, in a literal string, and as an
# escaped value under an escaped key is queried as Reindeer decodes it: its
# cfg changing re-vendors.
for target_line in 'target = """x86_64-unknown-linux-musl"""' "target = '''x86_64-unknown-linux-musl'''" \
  '"targ\u0065t" = "x86_64\u002dunknown-linux-musl"'; do
  { fixture_git -C "$repo" show HEAD:shim/third-party/rust/reindeer.toml
    printf '\n[platform.musl]\n%s\n' "$target_line"; } >"$reindeer_toml"
  regen target_form_new
  expect_vendored target_form_new
  ! grep -q 'not using the rust-deps cache' "$tmp/target_form_new.err" ||
    fail "target_form_new: cache declined for $target_line"
  regen target_form_again
  expect_restored target_form_again
  regen target_form_cfg FIXTURE_RUSTC_MUSL_CFG='target_feature="crt-static"'
  expect_ok target_form_cfg
  [[ $(calls target_form_cfg) == "vendor buckify buckify " ]] ||
    fail "target_form_cfg: the musl cfg did not change the key for $target_line"
done
fixture_git -C "$repo" checkout -q -- shim/third-party/rust/reindeer.toml

# Settings the key cannot cover, also under escaped keys, vendor, warn, and
# store nothing.
before=$(entries)
platform=$'[platform.linux-x86_64]\ntarget = "x86_64-unknown-linux-gnu"\n'
declined=(
  $'fixups_dir = "elsewhere"\n'"$platform"
  $'"fixups\\u005fdir" = "elsewhere"\n'"$platform"
  $'[cargo]\nrustc = "elsewhere"\n'"$platform"
  $'[cargo]\n"rust\\u0063" = "elsewhere"\n'"$platform"
  $'[cargo]\ncargo = "elsewhere"\n'"$platform"
  $'[vendor]\ngitignore_checksum_exclude = ["x"]\n'"$platform"
  $'unknown_setting = 1\n'"$platform"
  $'[cargo]\n'
)
for i in "${!declined[@]}"; do
  printf '%s' "${declined[$i]}" >"$reindeer_toml"
  regen "declined_$i"
  expect_vendored "declined_$i"
  grep -q "not using the rust-deps cache: .*reindeer.toml" "$tmp/declined_$i.err" ||
    fail "declined_$i: no warning for: ${declined[$i]}"
done
grep -q 'fixups_dir' "$tmp/declined_1.err" || fail "declined_1: the escaped key was not decoded"
grep -q 'cargo.rustc' "$tmp/declined_3.err" || fail "declined_3: the escaped key was not decoded"
grep -q 'no platform table' "$tmp/declined_7.err" || fail "declined_7: the platform-less case did not say so"
[[ $(entries) == "$before" ]] || fail "declined: cache changed from $before to $(entries) entries"
fixture_git -C "$repo" checkout -q -- shim/third-party/rust/reindeer.toml

# A key that cannot be computed vendors with a warning, restoring and storing
# nothing, although an entry for the checkout exists.
third_party=$repo/shim/third-party/rust
regen key_setup
expect_restored key_setup
before=$(stored)
expect_uncached() {
  expect_vendored "$1"
  grep -q "$2" "$tmp/$1.err" || fail "$1: no warning matching '$2': $(cat "$tmp/$1.err")"
  [[ $(stored) == "$before" ]] || fail "$1: cache changed from $before to $(stored) stored names"
}
regen key_metadata_fails FIXTURE_METADATA_FAIL="$tmp/metadata-calls"
expect_uncached key_metadata_fails 'its key could not be computed'
regen key_metadata_json FIXTURE_METADATA_JSON='{"packages":[{"manifest_path":"'
expect_uncached key_metadata_json 'its key could not be computed'
mkdir -p "$third_party/unreadable"
chmod 000 "$third_party/unreadable"
if [[ -r $third_party/unreadable ]]; then
  echo "test-rust-deps-cache: skipping the unreadable-directory cases: this user reads mode 000" >&2
  unreadable_works=
else
  unreadable_works=1
  regen key_unreadable
  expect_uncached key_unreadable 'its key could not be computed'
fi
chmod 755 "$third_party/unreadable"
rmdir -- "${third_party:?}/unreadable"
# A helper that prints a partial key, or declines without a reason.
cp -- "$repo/bootstrap/rust-deps-cache-key.rs" "$tmp/key-shim"
printf '#!/usr/bin/env bash\necho 0123abcd\n' >"$repo/bootstrap/rust-deps-cache-key.rs"
regen key_partial
expect_uncached key_partial 'not a key'
printf '#!/usr/bin/env bash\nexit 3\n' >"$repo/bootstrap/rust-deps-cache-key.rs"
regen key_declined_silently
expect_uncached key_declined_silently 'declined without giving a reason'
cp -- "$tmp/key-shim" "$repo/bootstrap/rust-deps-cache-key.rs"
# A Reindeer that leaves an unreadable directory in the third-party directory: the key
# computed afterwards fails, so nothing is stored.
if [[ -n $unreadable_works ]]; then
  mv -- "$repo/bootstrap/reindeer" "$tmp/reindeer-real"
  cat >"$repo/bootstrap/reindeer" <<EOF
#!/usr/bin/env bash
if [[ \$5 == vendor ]]; then
  mkdir -p "$third_party/broken"
  chmod 000 "$third_party/broken"
fi
exec "$tmp/reindeer-real" "\$@"
EOF
  chmod +x "$repo/bootstrap/reindeer"
  regen key_recheck_fails HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-recheck"
  expect_vendored key_recheck_fails
  grep -q 'could not be read again' "$tmp/key_recheck_fails.err" || fail "key_recheck_fails: no warning"
  [[ $(entries "$tmp/cache-recheck") == 0 ]] ||
    fail "key_recheck_fails: stored $(entries "$tmp/cache-recheck") entries"
  chmod 755 "$third_party/broken"
  rmdir -- "${third_party:?}/broken"
  mv -f -- "$tmp/reindeer-real" "$repo/bootstrap/reindeer"
fi
[[ -z $(fixture_git -C "$repo" status --porcelain) ]] ||
  fail "key cases: the fixture was left changed: $(fixture_git -C "$repo" status --porcelain)"

# A multi-line string elsewhere, as Hermit's buckfile_imports is, keeps the
# cache; and Hermit's own reindeer.toml is one the cache accepts.
{ fixture_git -C "$repo" show HEAD:shim/third-party/rust/reindeer.toml
  printf '\n[buck]\nbuckfile_imports = """\nload("@prelude//rust:cargo_package.bzl", "cargo")\n"""\n'; } >"$reindeer_toml"
regen multiline_new
expect_vendored multiline_new
grep -q 'not using the rust-deps cache' "$tmp/multiline_new.err" && fail "multiline_new: cache declined"
regen multiline_again
expect_restored multiline_again
fixture_git -C "$repo" checkout -q -- shim/third-party/rust/reindeer.toml
key_status=0
hermit_key=$("$script_dir/rust-deps-cache-key.rs" "$script_dir/..") || key_status=$?
[[ $key_status == 0 && $hermit_key =~ ^[0-9a-f]{64}$ ]] ||
  fail "the cache does not serve Hermit's own checkout (exit $key_status): $hermit_key"

# An eviction whose removal fails part-way: no restore may serve what is left.
regen evict_setup
expect_restored evict_setup
regen evict_fail PATH="$tmp/rmbin:$tmp/bin:$PATH" HERMIT_RUST_DEPS_CACHE_KEEP=1 \
  FIXTURE_CARGO_VERSION="cargo 1.0.3 (fixture)"
expect_vendored evict_fail
grep -q 'fixture rm: failed part-way' "$tmp/evict_fail.err" || fail "evict_fail: the failing rm never ran"
grep -q 'could not remove the retired entry' "$tmp/evict_fail.err" || fail "evict_fail: the failed removal was not reported"
[[ $(entries) == 1 ]] || fail "evict_fail: expected 1 cache entry, found $(entries)"
regen evict_after
expect_vendored evict_after
cmp -s "$tmp/first.tree" "$tmp/evict_after.tree" || fail "evict_after: tree differs from the first run's"
leftover=$(find "$cache/rust-deps" -mindepth 1 -maxdepth 1 -name '.evict-*' | wc -l)
[[ $leftover == 0 ]] || fail "evict_after: $leftover retired entries were not removed"

# An entry without its raw BUCK is absent to a restore (nothing is copied from
# it) and is replaced by the next publish.
regen marker_setup
expect_restored marker_setup
marker_key=$(sed -n 's/^restored .* from .*\/\([0-9a-f]\{64\}\)$/\1/p' "$tmp/marker_setup.err")
rm -f -- "$cache/rust-deps/${marker_key:?}/BUCK.reindeer"
regen markerless
expect_vendored markerless
grep -q 'restoring .* failed' "$tmp/markerless.err" && fail "markerless: a restore copied from the entry"
regen marker_replaced
expect_restored marker_replaced

# A publish paused with its staging complete, the staging aged past any age
# limit: another run publishes meanwhile and removes only abandoned staging
# (an unlocked directory, however new), leaving the paused one whole; resumed,
# it stores an entry that restores. The paused run uses its own clone, as two
# validation checkouts would.
stage_root=$tmp/cache-stage/rust-deps
fixture_git clone -q -- "$repo" "$tmp/repo-a"
mkdir -p "$stage_root/.staging-abandoned-000000/vendor"
regen_repo=$tmp/repo-a
regen stage_a HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-stage" PATH="$tmp/flockbin:$tmp/bin:$PATH" \
  FIXTURE_FLOCK_PAUSE="$stage_root" FIXTURE_CARGO_VERSION="cargo 1.0.4 (fixture)" &
stage_a_pid=$!
regen_repo=$repo
for _ in $(seq 600); do
  [[ ! -e $stage_root.paused ]] || break
  kill -0 "$stage_a_pid" 2>/dev/null || break
  sleep 0.1
done
if [[ ! -e $stage_root.paused ]]; then
  fail "stage: the first publish never paused: $(cat "$tmp/stage_a.err" 2>/dev/null)"
  touch -- "$stage_root.resume"
  wait "$stage_a_pid" || true
else
  live=$(find "$stage_root" -mindepth 1 -maxdepth 1 -name '.staging-*' ! -name '.staging-abandoned-*')
  touch -d '3 hours ago' -- "$live"
  regen stage_b HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-stage" FIXTURE_CARGO_VERSION="cargo 1.0.5 (fixture)"
  expect_vendored stage_b
  [[ -f $live/vendor/crate/source && -f $live/BUCK.reindeer ]] ||
    fail "stage_b: the paused publish's staging was damaged or removed"
  [[ ! -e $stage_root/.staging-abandoned-000000 ]] || fail "stage_b: abandoned staging was not removed"
  touch -- "$stage_root.resume"
  wait "$stage_a_pid" || true
  expect_vendored stage_a
  grep -q 'did not complete' "$tmp/stage_a.err" && fail "stage_a: the publish failed: $(cat "$tmp/stage_a.err")"
  [[ $(entries "$tmp/cache-stage") == 2 ]] ||
    fail "stage: expected 2 cache entries, found $(entries "$tmp/cache-stage")"
  regen_repo=$tmp/repo-a
  regen stage_a_again HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-stage" FIXTURE_CARGO_VERSION="cargo 1.0.4 (fixture)"
  regen_repo=$repo
  expect_restored stage_a_again
  cmp -s "$tmp/stage_a.tree" "$tmp/stage_a_again.tree" || fail "stage_a_again: restored tree differs from the generated one"
fi
leftover=$(find "$stage_root" -mindepth 1 -maxdepth 1 -name '.staging-*' | wc -l)
[[ $leftover == 0 ]] || fail "stage: $leftover staging directories were left"

if [[ $failures -ne 0 ]]; then
  echo "test-rust-deps-cache: $failures failure(s)" >&2
  exit 1
fi
echo "test-rust-deps-cache: ok"
