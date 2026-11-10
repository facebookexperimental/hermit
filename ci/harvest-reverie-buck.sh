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
# Placement is buck-harvest-only: binaries land in
# target/validate/buck-harvest/<name>, and nextest prepare repoints the
# recorded metadata at them (option b; Cargo's target files are never
# written). The manifest and buck-harvest/ are also mirrored into
# ignored/hermetic/split/target/validate/, the pinned prepare's declared
# input location (its /src/target mount shadows this checkout's target/).
# Binaries are accepted only after every RUNPATH entry resolves under
# /nix/store and each one ldd-resolves and lists inside the pinned root;
# the manifest is published only after that gate passes.
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

# Build against the pinned root's sysroot input so the binaries carry the
# nix RUNPATH their Cargo twins carry and execute where prepare lists them
# (inside the pinned root). A digest drift refuses in the staging script.
bash shim/modes/stage-pinned-sysroot
echo "harvest-reverie-buck: building ${#BUCK_TARGETS[@]} detcore targets with $BUCK2" >&2
"$BUCK2" build -c hermit.rust_sysroot=pinned-rust-sysroot --show-output "${BUCK_TARGETS[@]}" > "$BUILD_LOG" 2>&1 || {
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
  placed=false
  dest=target/validate/buck-harvest/${name}
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

# Route (ii) staging: prepare runs inside the pinned root, where
# /src/target is the split output mount (ignored/hermetic/split/target)
# and shadows this checkout's target/. Mirror the declared input --
# the manifest and the buck-harvest files it names, nothing else --
# into that location so prepare can read and hash-verify it there.
# Prepare's verification is unchanged: it refuses missing, mismatched
# or extra files by name against this same manifest.
PINNED_VALIDATE=ignored/hermetic/split/target/validate
mkdir -p "$PINNED_VALIDATE"
rm -rf "$PINNED_VALIDATE/buck-harvest" "$PINNED_VALIDATE/harvested-files.json"
mkdir -p "$PINNED_VALIDATE/buck-harvest"
printf '%s' "$ENTRIES" | while IFS='|' read -r _target name _src dest _placed _sha _bytes _listed; do
  case $dest in
    target/validate/buck-harvest/*)
      cp -- "$dest" "$PINNED_VALIDATE/buck-harvest/$name"
      ;;
    *)
      echo "harvest-reverie-buck: refusing to mirror non-buck-harvest dest $dest" >&2
      exit 2
      ;;
  esac
done
echo "harvest-reverie-buck: staged buck-harvest into $PINNED_VALIDATE (manifest withheld until acceptance)" >&2

# Acceptance, before anything is published: every RUNPATH entry must lie
# under /nix/store (no host paths, however inert), and every harvested
# binary must resolve and list INSIDE the pinned root, where prepare runs
# it. A host-linked binary passes every host check above and then fails
# prepare with exit 127, so prove both here, where failures name this
# producer. The manifest is written only after this gate passes, so a
# failing harvest leaves no publishable input behind.
mkdir -p ignored/hermetic/split/cargo
for entry in "${TARGETS[@]}"; do
  name=${entry##*|}
  rpath=$(readelf -d "target/validate/buck-harvest/$name" | sed -n 's/.*RUNPATH.*\[\(.*\)\]/\1/p')
  [[ -n $rpath ]] || { echo "harvest-reverie-buck: $name has no RUNPATH" >&2; exit 2; }
  IFS=: read -ra rpath_entries <<<"$rpath"
  for dir in "${rpath_entries[@]}"; do
    [[ $dir == /nix/store/* ]] || {
      echo "harvest-reverie-buck: $name RUNPATH entry outside /nix/store: $dir" >&2
      exit 2
    }
  done
  ./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split \
    --src-rw --cargo-home ignored/hermetic/split/cargo -- \
    bash -c "ldd /src/target/validate/buck-harvest/$name | grep -q 'not found' && exit 1; /src/target/validate/buck-harvest/$name --list >/dev/null" || {
    echo "harvest-reverie-buck: $name does not resolve/execute inside the pinned root" >&2
    exit 2
  }
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
cp -- "$MANIFEST" "$PINNED_VALIDATE/harvested-files.json"
echo "harvest-reverie-buck: done" >&2
