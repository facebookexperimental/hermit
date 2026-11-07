#!/usr/bin/env bash
# Harvest the Reverie validate test set from a Buck build (M2 Cut 3).
#
# Builds the Reverie integration-test targets with buck2, copies ONLY the
# files readers read into the existing target/validate paths, executes each
# harvested binary with --list so the cohort is proven to run, and writes
# the harvested-file manifest at target/validate/harvested-files.json.
#
# Placement is buck-harvest-only by default: binaries land in
# target/validate/buck-harvest/<name>, and nextest prepare repoints the
# recorded metadata at them (option b; Cargo's target files are never
# written). HARVEST_ALLOW_PLACED=1 is the explicit legacy opt-in that
# instead replaces an existing Cargo counterpart in
# target/validate/deps/; prepare refuses any manifest produced that way.
#
# The OSS shim reports no sanitizer (shim/build_defs/sanitizers.bzl), so in
# this repository the Buck cohort is the unsanitized OSS configuration; in
# fbcode the same targets build under the asan-ubsan-dev configuration.
# Cut 4 compares binary sets, --list case sets and outcome counts, never
# bytes, so the cohort difference is named rather than hidden.
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd "$ROOT_DIR"

MANIFEST=target/validate/harvested-files.json

# Resolve buck2: an explicit HERMIT_VALIDATE_BUCK2 wins. Otherwise prefer a
# real buck2 on PATH, but the PATH entry in a fresh validate checkout is a
# DotSlash shim that cannot read its descriptor there, so fall back to the
# DotSlash cache's extracted binary. Refuse if none exists.
if [[ -n ${HERMIT_VALIDATE_BUCK2:-} ]]; then
  BUCK2=$HERMIT_VALIDATE_BUCK2
elif command -v buck2 >/dev/null 2>&1 && buck2 --version >/dev/null 2>&1; then
  BUCK2=buck2
else
  BUCK2=$(find "$HOME/.cache/dotslash" -maxdepth 4 -name 'buck2-*-unknown-linux-*' -type f 2>/dev/null | sort | tail -n 1)
  if [[ -z $BUCK2 ]]; then
    echo "harvest-reverie-buck: no usable buck2 (set HERMIT_VALIDATE_BUCK2)" >&2
    exit 2
  fi
fi

# target| harvested name (the Cargo test-binary stem)
TARGETS=(
  "reverie//:generic-basics|basics"
  "reverie//:generic-exit|exit"
  "reverie//:generic-stat|stat"
  "reverie//:generic-vdso|vdso"
  "reverie//:generic-inject-then-tail|inject_then_tail"
  "reverie//:generic-thread-start|thread_start"
  "reverie//:generic-initial-stack-pointer|initial_stack_pointer"
  "reverie//:generic-at-random|at_random"
  "reverie//:generic-stack|stack"
  "reverie//:generic-suppression|suppression"
)

if [[ ! -f shim/third-party/rust/BUCK ]]; then
  ./bootstrap/regenerate-rust-deps
fi

mkdir -p target/validate/buck-harvest
# Never bind a stale sibling library: this run's copy is recreated below
# only when the Buck output actually provides one.
rm -f target/validate/buck-harvest/libreverie_examples.so
BUILD_LOG=$(mktemp target/validate/buck-harvest/build.XXXXXX.log)
trap 'rm -f "$BUILD_LOG"' EXIT

BUCK_TARGETS=()
for entry in "${TARGETS[@]}"; do
  BUCK_TARGETS+=("${entry%%|*}")
done

echo "harvest-reverie-buck: building ${#BUCK_TARGETS[@]} Reverie targets with $BUCK2" >&2
"$BUCK2" build --show-output "${BUCK_TARGETS[@]}" > "$BUILD_LOG" 2>&1 || {
  tail -n 30 "$BUILD_LOG" >&2
  echo "harvest-reverie-buck: buck build failed" >&2
  exit 2
}

ENTRIES=""
for entry in "${TARGETS[@]}"; do
  target=${entry%%|*}
  name=${entry##*|}
  src=$(grep -F "$target " "$BUILD_LOG" | tail -n 1 | awk '{print $2}')
  if [[ -z $src || ! -f $src ]]; then
    echo "harvest-reverie-buck: no build output for $target" >&2
    exit 2
  fi
  # Place over the Cargo counterpart when exactly one exists, else
  # buck-harvest/. Multiple stale counterparts are a refusal: picking the
  # first glob match could replace a binary prepare does not name.
  dest=""
  placed=false
  matches=()
  for candidate in target/validate/deps/${name}-*; do
    if [[ -f $candidate && -x $candidate && $candidate != *.d ]]; then
      matches+=("$candidate")
    fi
  done
  if [[ ${#matches[@]} -gt 1 ]]; then
    echo "harvest-reverie-buck: multiple Cargo counterparts for $name: ${matches[*]}" >&2
    exit 2
  fi
  # Placement over a Cargo counterpart only behind the explicit legacy
  # opt-in HARVEST_ALLOW_PLACED=1; the default is buck-harvest/ only.
  if [[ ${HARVEST_ALLOW_PLACED:-} == 1 && ${#matches[@]} -eq 1 ]]; then
    dest=${matches[0]}
    placed=true
  fi
  if [[ -z $dest ]]; then
    dest=target/validate/buck-harvest/${name}
  fi
  cp -- "$src" "$dest"
  chmod 755 "$dest"
  touch "$dest"
  # The sibling shared library readers resolve beside the binary (and in
  # deps/ when the binary itself lives there).
  src_dir=$(dirname -- "$src")
  if [[ -f $src_dir/libreverie_examples.so ]]; then
    cp -- "$src_dir/libreverie_examples.so" "$(dirname -- "$dest")/libreverie_examples.so"
    touch "$(dirname -- "$dest")/libreverie_examples.so"
  fi
  # Prove the cohort runs: list its cases from the harvested path itself.
  listed=$("$dest" --list 2>/dev/null | grep -c ': test$' || true)
  if [[ $listed -lt 1 ]]; then
    echo "harvest-reverie-buck: $dest listed no test cases" >&2
    exit 2
  fi
  sha=$(sha256sum "$dest" | awk '{print $1}')
  bytes=$(stat -c %s "$dest")
  ENTRIES="${ENTRIES}${target}|${name}|${src}|${dest}|${placed}|${sha}|${bytes}|${listed}"$'\n'
  echo "harvest-reverie-buck: $name <- $src => $dest (placed=$placed, $listed cases)" >&2
done

printf '%s' "$ENTRIES" | python3 -c '
import json, sys
files = []
for line in sys.stdin:
    line = line.rstrip("\n")
    if not line:
        continue
    target, name, src, dest, placed, sha, size, listed = line.split("|")
    files.append({
        "target": target,
        "name": name,
        "buck_output": src,
        "harvested_to": dest,
        "placed_over_cargo": placed == "true",
        "sha256": sha,
        "bytes": int(size),
        "listed_cases": int(listed),
    })
manifest = {
    "schema": 1,
    "producer": "build.reverie_buck (M2 Cut 3)",
    "cohort": "oss-unsanitized (shim reports no sanitizer); fbcode builds the same targets under asan-ubsan-dev",
    "files": files,
}
with open("target/validate/harvested-files.json", "w") as fh:
    json.dump(manifest, fh, indent=2)
    fh.write("\n")
print(f"harvest-reverie-buck: wrote target/validate/harvested-files.json with {len(files)} files", file=sys.stderr)
'
echo "harvest-reverie-buck: done" >&2
