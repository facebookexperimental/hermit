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
#   scripts/lib/prelude-cache-key.sh --write    # restamp all consumers
#
# Run --write after editing scripts/lib/rust_script_prelude.rs. The --check mode
# is wired into scripts/check-script-sigpipe.sh so a forgotten restamp is caught.
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

mode="--check"
[[ ${1:-} == "--write" || ${1:-} == "--check" ]] && mode="$1"

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
