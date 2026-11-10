#!/usr/bin/env bash
# Harvest Hermit's own Buck-built detcore test binaries (dh-iz46e).
#
# Builds the detcore rust_unittest targets (tests_misc, tests_parallelism,
# tests_time in detcore/BUCK; Cargo twins in detcore/tests/Cargo.toml) with
# buck2, copies ONLY the files readers read into the existing
# target/validate paths, executes each harvested binary with --list so the
# cohort is proven to run, and writes the harvested-file manifest at
# target/validate/harvested-files.json. Nextest prepare substitutes these
# binaries into the test.detcore_misc/parallel/time selections only after
# exact name-set and Cargo-twin case-set equality checks.
#
# Placement is buck-harvest-only by default: binaries land in
# target/validate/buck-harvest/<name>, and nextest prepare repoints the
# recorded metadata at them (option b; Cargo's target files are never
# written). The manifest and buck-harvest/ are also mirrored into
# ignored/hermetic/split/target/validate/, the pinned prepare's declared
# input location (its /src/target mount shadows this checkout's target/).
# HARVEST_ALLOW_PLACED=1 is the explicit legacy opt-in that
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
  "hermit//detcore:tests_misc|tests_misc"
  "hermit//detcore:tests_parallelism|tests_parallelism"
  "hermit//detcore:tests_time|tests_time"
)

if [[ ! -f shim/third-party/rust/BUCK ]]; then
  ./bootstrap/regenerate-rust-deps
fi

mkdir -p target/validate/buck-harvest
# The harvest directory is producer-owned: clear prior-run artifacts (in
# particular binaries from a previous target set) so prepare's
# no-undeclared-files check sees exactly this run's manifest.
find target/validate/buck-harvest -maxdepth 1 -type f -delete
# Retain the buck2 build log OUTSIDE buck-harvest/ (prepare refuses any
# undeclared file inside it) so the run's cache hit rate stays measurable
# from retained evidence instead of cache-byte deltas; overwritten per run.
BUILD_LOG=target/validate/buck-harvest-build.log
: > "$BUILD_LOG"

BUCK_TARGETS=()
for entry in "${TARGETS[@]}"; do
  BUCK_TARGETS+=("${entry%%|*}")
done

echo "harvest-reverie-buck: building ${#BUCK_TARGETS[@]} detcore targets with $BUCK2" >&2
"$BUCK2" build --show-output "${BUCK_TARGETS[@]}" > "$BUILD_LOG" 2>&1 || {
  tail -n 30 "$BUILD_LOG" >&2
  echo "harvest-reverie-buck: buck build failed (log retained at $BUILD_LOG)" >&2
  exit 2
}
# Surface the build's own cache counters in this node's log; the full log
# stays at $BUILD_LOG for anything the summary lines do not carry.
grep -E 'Cache hits|Jobs completed|BUILD (SUCCEEDED|FAILED)|Network:' "$BUILD_LOG" >&2 || true

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
    "producer": "build.reverie_buck (dh-iz46e detcore harvest)",
    "cohort": "oss-unsanitized (shim reports no sanitizer); fbcode builds the same targets under asan-ubsan-dev",
    "files": files,
}
with open("target/validate/harvested-files.json", "w") as fh:
    json.dump(manifest, fh, indent=2)
    fh.write("\n")
print(f"harvest-reverie-buck: wrote target/validate/harvested-files.json with {len(files)} files", file=sys.stderr)
'
# Route (ii) staging: prepare runs inside the pinned root, where
# /src/target is the split output mount (ignored/hermetic/split/target)
# and shadows this checkout's target/. Mirror the declared input --
# the manifest and the buck-harvest files it names, nothing else --
# into that location so prepare can read and hash-verify it there.
# Prepare's verification is unchanged: it refuses missing, mismatched
# or extra files by name against this same manifest.
PINNED_VALIDATE=ignored/hermetic/split/target/validate
mkdir -p "$PINNED_VALIDATE"
rm -rf "$PINNED_VALIDATE/buck-harvest"
mkdir -p "$PINNED_VALIDATE/buck-harvest"
cp -- "$MANIFEST" "$PINNED_VALIDATE/harvested-files.json"
printf '%s' "$ENTRIES" | while IFS='|' read -r _target name _src dest _placed _sha _bytes _listed; do
  case $dest in
    target/validate/buck-harvest/*)
      cp -- "$dest" "$PINNED_VALIDATE/buck-harvest/$name"
      ;;
  esac
done
echo "harvest-reverie-buck: staged manifest and buck-harvest into $PINNED_VALIDATE" >&2
echo "harvest-reverie-buck: done" >&2
