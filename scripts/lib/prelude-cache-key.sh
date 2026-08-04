#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# prelude-cache-key.sh — keep rust-script consumers rebuilding when the shared
# prelude changes.
#
# THE TRAP: rust-script (0.36) decides a cached binary is fresh by comparing the
# built binary's mtime against ONLY the main script file's mtime (and the
# generated Cargo manifest's). It never looks at `#[path = "..."]`-included
# modules. Our standalone scripts all `#[path]`-include
# scripts/lib/rust_script_prelude.rs, so editing the prelude does NOT bust their
# caches: a machine whose ~/.cache/rust-script warmed before the change keeps
# running the pre-change binary (e.g. `manifest-cli list | head` exits 141 even
# though the SIGPIPE handler is present in prelude source).
#
# THE FIX: stamp a short digest of the prelude onto each consumer's
# `mod rust_script_prelude;` line. Because the digest lives in the consumer's
# OWN bytes, any prelude change is accompanied by a consumer content change,
# which (a) propagates through git as a fresh mtime + new content on checkout and
# (b) is exactly what rust-script's freshness check consults — so the consumer
# rebuilds and picks up the new prelude on its next run.
#
# Usage:
#   scripts/lib/prelude-cache-key.sh            # --check (default): fail if stale
#   scripts/lib/prelude-cache-key.sh --check
#   scripts/lib/prelude-cache-key.sh --check-runtime
#   scripts/lib/prelude-cache-key.sh --write    # restamp all consumers
#
# Run --write after editing scripts/lib/rust_script_prelude.rs. The --check mode
# is wired into scripts/check-script-sigpipe.sh so a forgotten restamp is caught.
# --check-runtime additionally resolves the exact release binary rust-script
# would execute for every consumer and fails if that binary predates the main
# script, generated Cargo manifest, or shared prelude. It is the observable
# agent-side predicate: its FRESH lines name the digest, binary, and timestamps.
#
# Effective invalidation mechanisms:
#   rust-script --force SCRIPT [SCRIPT_ARGS...]  # rebuild one script now
#   rust-script --clear-cache                    # remove all cached binaries
#   XDG_CACHE_HOME="$(mktemp -d)" SCRIPT ...     # execute with a cold cache
# `--write` is the repository mechanism: it changes every consumer's own bytes,
# so its next normal invocation is newer than the cached binary and rebuilds.
#
# This is a bash script, not a rust-script, on purpose: it maintains the very
# cache key the rust-script consumers depend on, so it must run correctly even
# when their caches are stale (no bootstrap-through-the-bug).
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

PRELUDE="scripts/lib/rust_script_prelude.rs"
MARKER="mod rust_script_prelude;"
TAG="rust-script cache-key:"

[[ -f $PRELUDE ]] || { echo "prelude-cache-key.sh: missing $PRELUDE" >&2; exit 2; }
command -v sha1sum >/dev/null 2>&1 || { echo "prelude-cache-key.sh: sha1sum required" >&2; exit 2; }

digest() { sha1sum "$PRELUDE" | cut -c1-12; }

# A consumer is a rust-script (shebang `#!/usr/bin/env rust-script`) that
# `#[path]`-includes the prelude as a module. Discovered dynamically so a new
# consumer is covered automatically. The prelude itself and the rustc-compiled
# sigpipe_smoke fixture (no rust-script cache) are excluded.
consumers() {
  local f
  while IFS= read -r f; do
    [[ $f == "$PRELUDE" ]] && continue
    [[ $(head -1 "$f") == "#!/usr/bin/env rust-script" ]] || continue
    printf '%s\n' "$f"
  done < <(grep -rl --include='*.rs' -F "$MARKER" scripts tests 2>/dev/null | sort)
}

stamped_line() { printf '%s // %s %s (regen: scripts/lib/prelude-cache-key.sh --write)' "$MARKER" "$TAG" "$1"; }

mode="${1:---check}"
case "$mode" in
  --write|--check|--check-runtime) ;;
  *)
    echo "Usage: $0 [--check | --check-runtime | --write]" >&2
    exit 2
    ;;
esac

want="$(digest)"
mapfile -t files < <(consumers)
[[ ${#files[@]} -gt 0 ]] || { echo "prelude-cache-key.sh: no rust-script consumers found (looked for '$MARKER')" >&2; exit 2; }

if [[ $mode == "--write" ]]; then
  new_line="$(stamped_line "$want")"
  changed=0
  for f in "${files[@]}"; do
    # Replace the whole marker line (bare or previously stamped) in place.
    tmp="$(mktemp)"
    awk -v marker="$MARKER" -v repl="$new_line" '
      index($0, marker) == 1 { print repl; next }
      { print }
    ' "$f" > "$tmp"
    if ! cmp -s "$f" "$tmp"; then cat "$tmp" > "$f"; changed=$((changed+1)); echo "stamped $f -> $want"; fi
    rm -f "$tmp"
  done
  echo "prelude-cache-key.sh: OK — ${#files[@]} consumer(s), $changed updated, cache-key $want"
  exit 0
fi

# --check
stale=()
for f in "${files[@]}"; do
  have="$(awk -v marker="$MARKER" -v tag="$TAG" '
    index($0, marker) == 1 {
      i = index($0, tag)
      if (i == 0) { print "MISSING"; exit }
      rest = substr($0, i + length(tag))
      n = split(rest, a, " ")
      for (k = 1; k <= n; k++) if (a[k] != "") { print a[k]; exit }
      print "MISSING"; exit
    }
  ' "$f")"
  [[ $have == "$want" ]] || stale+=("$f (have: ${have:-none}, want: $want)")
done

if [[ ${#stale[@]} -gt 0 ]]; then
  {
    echo "prelude-cache-key.sh: FAIL — prelude cache-key is stale in ${#stale[@]} consumer(s):"
    printf '  %s\n' "${stale[@]}"
    echo "  $PRELUDE changed but consumers were not restamped, so rust-script would"
    echo "  serve stale cached binaries on warm-cache machines."
    echo "  Fix: scripts/lib/prelude-cache-key.sh --write   (then commit the result)"
  } >&2
  exit 1
fi
echo "prelude-cache-key.sh: OK — ${#files[@]} consumer(s) carry current cache-key $want"

[[ $mode == "--check-runtime" ]] || exit 0

# Mirror rust-script 0.36's Linux cache root. `dirs::cache_dir()` uses
# XDG_CACHE_HOME when set and otherwise HOME/.cache. Authoritative Hermit tools
# are invoked through their shebang and therefore use the release cache.
command -v rust-script >/dev/null 2>&1 || {
  echo "prelude-cache-key.sh: rust-script is required for --check-runtime" >&2
  exit 2
}
command -v stat >/dev/null 2>&1 || {
  echo "prelude-cache-key.sh: stat is required for --check-runtime" >&2
  exit 2
}

cache_home="${XDG_CACHE_HOME:-${HOME:?HOME must be set}/.cache}"
binary_dir="$cache_home/rust-script/binaries/release"

# rust-script compares the cached binary's creation time (falling back to its
# mtime) with the main script and generated manifest. We additionally compare
# with the shared prelude: that is the input rust-script itself ignores.
file_mtime() { stat -c '%y' "$1"; }
binary_build_time() {
  local birth
  birth="$(stat -c '%w' "$1")"
  if [[ $birth == '-' ]]; then file_mtime "$1"; else printf '%s\n' "$birth"; fi
}

fresh=0
cold=0
runtime_stale=0
for f in "${files[@]}"; do
  project="$(rust-script --package "$f")"
  manifest="$project/Cargo.toml"
  [[ -f $manifest ]] || {
    echo "prelude-cache-key.sh: FAIL — rust-script did not generate $manifest for $f" >&2
    runtime_stale=$((runtime_stale + 1))
    continue
  }
  bin_name="$(awk '
    $0 == "[[bin]]" { in_bin = 1; next }
    in_bin && $1 == "name" && $2 == "=" {
      gsub(/"/, "", $3); print $3; exit
    }
  ' "$manifest")"
  [[ -n $bin_name ]] || {
    echo "prelude-cache-key.sh: FAIL — cannot read [[bin]].name from $manifest" >&2
    runtime_stale=$((runtime_stale + 1))
    continue
  }
  binary="$binary_dir/$bin_name"
  if [[ ! -f $binary ]]; then
    echo "COLD  $f key=$want binary=$binary (absent; next invocation must compile)"
    cold=$((cold + 1))
    continue
  fi

  built="$(binary_build_time "$binary")"
  consumer_time="$(file_mtime "$f")"
  manifest_time="$(file_mtime "$manifest")"
  prelude_time="$(file_mtime "$PRELUDE")"
  newest_input="$consumer_time"
  [[ $manifest_time > $newest_input ]] && newest_input="$manifest_time"
  [[ $prelude_time > $newest_input ]] && newest_input="$prelude_time"

  if [[ $built < $consumer_time || $built < $manifest_time || $built < $prelude_time ]]; then
    {
      echo "STALE $f key=$want"
      echo "      binary=$binary"
      echo "      built=$built newest-input=$newest_input"
      echo "      consumer=$consumer_time manifest=$manifest_time prelude=$prelude_time"
    } >&2
    runtime_stale=$((runtime_stale + 1))
  else
    echo "FRESH $f key=$want binary=$binary built=$built newest-input=$newest_input"
    fresh=$((fresh + 1))
  fi
done

echo "prelude-cache-key.sh: runtime summary — fresh=$fresh cold=$cold stale=$runtime_stale cache=$binary_dir"
if (( runtime_stale > 0 )); then
  {
    echo "prelude-cache-key.sh: FAIL — $runtime_stale cached executable(s) can run stale prelude code."
    echo "  Rebuild one: rust-script --force SCRIPT [SCRIPT_ARGS...]"
    echo "  Invalidate all: rust-script --clear-cache"
    # The command substitution is intentionally printed for the agent to run.
    # shellcheck disable=SC2016
    echo '  Prove cold: XDG_CACHE_HOME="$(mktemp -d)" SCRIPT [SCRIPT_ARGS...]'
    echo "  Then rerun: scripts/lib/prelude-cache-key.sh --check-runtime"
  } >&2
  exit 1
fi
