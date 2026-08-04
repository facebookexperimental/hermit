#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# check-detcore-backend-abstraction.sh
# ------------------------------------
# Enforce the DETCORE BACKEND-ABSTRACTION COMMANDMENT.
#
# Commandment (see detcore/Cargo.toml and detcore/src/lib.rs):
#
#   The detcore core library depends ONLY on the abstract Reverie interface
#   crate (`reverie`, whose package is `reverie-core`). It MUST NEVER depend on
#   another `reverie-*` crate. Backends are selected and instantiated
#   EXCLUSIVELY by the `hermit-cli` package, which constructs a detcore tool and
#   runs it against a chosen backend. There are no backend-specific hacks in
#   detcore.
#
# Why: Hermit follows Reverie's abstract instrumentation model. A backend
# dependency in detcore would couple the determinism engine to one tracing
# mechanism and break the clean abstraction boundary.
#
# What this lint checks:
#   1. detcore/Cargo.toml: no non-core Reverie crate appears in any NON-test
#      dependency table ([dependencies], [build-dependencies],
#      [target.*.dependencies]).
#   2. detcore/src/**: no non-core Reverie crate is imported or referenced from
#      the library source (use / extern crate / path `reverie_ptrace::` etc.).
#
# What this lint intentionally ALLOWS:
#   - Backend crates under [dev-dependencies] and in detcore/tests/**. Detcore's
#     own integration tests must drive a real tracer to exercise the tool; that
#     test-only coupling does not leak into the shipped `detcore` rlib or its
#     consumers.
#
# Exit codes:
#   0  boundary intact
#   1  violation detected (backend dep or import in the core library)
#   2  usage / environment error

set -uo pipefail

repo_root_override=""
skip_negative_control=false
while (($# > 0)); do
    case "$1" in
        --repo-root)
            if (($# < 2)); then
                echo "error: --repo-root requires a path" >&2
                exit 2
            fi
            repo_root_override=$2
            shift 2
            ;;
        --skip-negative-control)
            skip_negative_control=true
            shift
            ;;
        -h|--help)
            echo "usage: $0 [--repo-root PATH] [--skip-negative-control]"
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

# --- output helpers ----------------------------------------------------------

is_tty() { [[ -t 1 ]]; }
if is_tty; then
    C_RED=$'\033[31m'; C_GRN=$'\033[32m'; C_DIM=$'\033[2m'; C_RST=$'\033[0m'
else
    C_RED=""; C_GRN=""; C_DIM=""; C_RST=""
fi
info() { echo "${C_DIM}info:${C_RST} $*"; }
ok()   { echo "${C_GRN}ok:${C_RST}   $*"; }
err()  { echo "${C_RED}error:${C_RST} $*" >&2; }

# --- locate the repo and detcore ---------------------------------------------

script_dir() { cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd; }
if [[ -n $repo_root_override ]]; then
    REPO_ROOT="$(cd -- "$repo_root_override" && pwd)"
else
    REPO_ROOT="$(cd -- "$(script_dir)/.." && pwd)"
fi

readonly DETCORE_MANIFEST="$REPO_ROOT/detcore/Cargo.toml"
readonly DETCORE_SRC="$REPO_ROOT/detcore/src"

if [[ ! -f $DETCORE_MANIFEST ]]; then
    err "detcore manifest not found: $DETCORE_MANIFEST"
    exit 2
fi
if [[ ! -d $DETCORE_SRC ]]; then
    err "detcore source directory not found: $DETCORE_SRC"
    exit 2
fi

# Derive every non-core Reverie crate named by a workspace member. This set is
# intentionally broader than today's execution backends: the commandment says
# detcore depends only on reverie-core, so a direct dependency on any other
# Reverie implementation/support crate is a boundary violation. A new
# reverie-* dependency automatically joins the prohibited set without editing
# this lint.
backend_output="$(python3 - "$REPO_ROOT" <<'PY'
import glob
import sys
import tomllib
from pathlib import Path

root = Path(sys.argv[1])

def load(path: Path):
    with path.open("rb") as source:
        return tomllib.load(source)

root_manifest = load(root / "Cargo.toml")
members = root_manifest.get("workspace", {}).get("members", [])
manifest_paths = {root / "Cargo.toml"}
for pattern in members:
    for candidate_name in glob.glob(str(root / pattern)):
        candidate = Path(candidate_name)
        manifest = candidate if candidate.name == "Cargo.toml" else candidate / "Cargo.toml"
        if manifest.is_file():
            manifest_paths.add(manifest)

def dependency_tables(document):
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = document.get(key)
        if isinstance(table, dict):
            yield table
    workspace = document.get("workspace")
    if isinstance(workspace, dict):
        table = workspace.get("dependencies")
        if isinstance(table, dict):
            yield table
    target = document.get("target")
    if isinstance(target, dict):
        for target_table in target.values():
            if not isinstance(target_table, dict):
                continue
            for key in ("dependencies", "dev-dependencies", "build-dependencies"):
                table = target_table.get(key)
                if isinstance(table, dict):
                    yield table

crates = set()
for manifest in manifest_paths:
    document = load(manifest)
    for table in dependency_tables(document):
        for dependency_key, specification in table.items():
            package = dependency_key
            if isinstance(specification, dict):
                package = specification.get("package", dependency_key)
            if package.startswith("reverie-") and package != "reverie-core":
                crates.add(package)

print("\n".join(sorted(crates)))
PY
)" || {
    err "failed to derive non-core Reverie crates from workspace Cargo manifests"
    exit 2
}

if [[ -z $backend_output ]]; then
    err "workspace declares no non-core reverie-* crates; refusing a vacuous abstraction check"
    exit 2
fi
mapfile -t BACKEND_CRATES <<< "$backend_output"
BACKEND_MODS_RE=""
for backend in "${BACKEND_CRATES[@]}"; do
    module=${backend//-/_}
    BACKEND_MODS_RE+="${BACKEND_MODS_RE:+|}${module}"
done
readonly BACKEND_MODS_RE
info "derived prohibited Reverie crates from workspace: ${BACKEND_CRATES[*]}"

violations=0

# --- 1. manifest: no backend in non-test dependency tables -------------------
#
# Walk Cargo.toml tracking the current table header. A dependency line is
# flagged when its key (or a `package = "..."` rename target) names a backend
# crate AND the current table is a dependency table that is not
# [dev-dependencies]. Commented lines are ignored.

manifest_hits="$(
    awk -v backends="${BACKEND_CRATES[*]}" '
        function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }
        BEGIN {
            n = split(backends, arr, " ")
            for (i = 1; i <= n; i++) banned[arr[i]] = 1
            insec = 0
        }
        # Table header line, e.g. [dependencies] or a target-scoped dep table.
        /^[ \t]*\[/ {
            hdr = trim($0)
            # A dependency table, but NOT the test-only dev-dependencies table.
            insec = (hdr ~ /dependencies\][ \t]*$/ && hdr !~ /dev-dependencies/)
            next
        }
        # Skip blank and comment lines.
        /^[ \t]*#/ { next }
        /^[ \t]*$/ { next }
        insec {
            key = $0; sub(/=.*/, "", key); key = trim(key)
            # strip optional surrounding quotes from the dependency key
            gsub(/"/, "", key)
            pkg = ""
            if (match($0, /package[ \t]*=[ \t]*"[^"]+"/)) {
                pkg = substr($0, RSTART, RLENGTH)
                sub(/.*package[ \t]*=[ \t]*"/, "", pkg); sub(/".*/, "", pkg)
            }
            if ((key in banned) || (pkg != "" && (pkg in banned)))
                printf "  %d: %s\n", FNR, trim($0)
        }
    ' "$DETCORE_MANIFEST"
)"

if [[ -n $manifest_hits ]]; then
    err "detcore/Cargo.toml declares a non-core Reverie crate in a non-test dependency table:"
    printf '%s\n' "$manifest_hits" >&2
    err "detcore must depend only on the abstract 'reverie' crate. Move backend wiring to hermit-cli."
    ((violations++))
else
    ok "detcore/Cargo.toml: no non-core Reverie crate in runtime/build dependency tables"
fi

# --- 2. library source: no backend imports -----------------------------------

src_hits="$(grep -rnE "(^|[^A-Za-z0-9_])(${BACKEND_MODS_RE})([^A-Za-z0-9_]|$)" \
    "$DETCORE_SRC" 2>/dev/null | grep -vE '^\s*[^:]+:[0-9]+:\s*//' || true)"

if [[ -n $src_hits ]]; then
    err "detcore/src references a non-core Reverie crate module:"
    printf '%s\n' "$src_hits" >&2
    err "detcore library code must use only the abstract 'reverie' interfaces."
    ((violations++))
else
    ok "detcore/src: no imports from derived non-core Reverie crates"
fi

# Exercise the real checker against scratch detcore copies. This is a negative
# control, not a mock: each recursive invocation re-derives its prohibited set
# from the scratch workspace after the planted dependency is added.
run_negative_controls() {
    local scratch backend output status
    local -a control_crates=("${BACKEND_CRATES[@]}")

    # e9patch is not currently declared by a workspace member, so it cannot be
    # part of the derived set yet. Keep it as a sentinel until it is declared;
    # once present, the derived list supplies it and this append is skipped.
    if [[ " ${control_crates[*]} " != *" reverie-e9patch "* ]]; then
        control_crates+=(reverie-e9patch)
    fi

    for backend in "${control_crates[@]}"; do
        if ! scratch=$(mktemp -d); then
            err "negative control could not create a scratch directory"
            return 1
        fi
        if ! cp -a "$REPO_ROOT/detcore" "$scratch/detcore" ||
           ! printf '[workspace]\nmembers = ["detcore"]\nresolver = "2"\n' \
                > "$scratch/Cargo.toml" ||
           ! printf '\n[target.'"'"'cfg(any())'"'"'.dependencies]\n%s = "0.2.0"\n' \
                "$backend" >> "$scratch/detcore/Cargo.toml"; then
            err "negative control could not prepare the $backend scratch copy"
            rm -rf -- "$scratch"
            return 1
        fi

        output="$("${BASH_SOURCE[0]}" --repo-root "$scratch" \
            --skip-negative-control 2>&1)"
        status=$?
        rm -rf -- "$scratch"

        if ((status != 1)); then
            err "negative control for $backend returned $status, expected 1"
            printf '%s\n' "$output" >&2
            return 1
        fi
        if ! grep -Fq "$backend" <<< "$output"; then
            err "negative control failed without identifying $backend"
            printf '%s\n' "$output" >&2
            return 1
        fi
        ok "negative control: planted $backend dependency was rejected"
    done
}

# --- summary -----------------------------------------------------------------

echo
if ((violations > 0)); then
    err "backend-abstraction commandment VIOLATED ($violations check(s) failed)."
    err "See detcore/src/lib.rs and detcore/Cargo.toml for the commandment."
    exit 1
fi
if ! $skip_negative_control && ! run_negative_controls; then
    err "backend-abstraction negative control FAILED; the lint is not trustworthy."
    exit 2
fi
ok "backend-abstraction commandment intact: detcore depends only on abstract 'reverie'."
exit 0
