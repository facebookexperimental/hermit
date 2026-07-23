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
#   crate (`reverie`). It MUST NEVER depend on a concrete Reverie backend
#   (`reverie-ptrace`, `reverie-dbi`, `reverie-kvm`). Backends are selected and
#   instantiated EXCLUSIVELY by the `hermit-cli` package, which constructs a
#   detcore tool and runs it against a chosen backend. There are no
#   backend-specific hacks in detcore.
#
# Why: Hermit follows Reverie's abstract instrumentation model. A backend
# dependency in detcore would couple the determinism engine to one tracing
# mechanism (ptrace/dbi/kvm) and break the clean abstraction boundary.
#
# What this lint checks:
#   1. detcore/Cargo.toml: no backend crate appears in any NON-test dependency
#      table ([dependencies], [build-dependencies], [target.*.dependencies]).
#   2. detcore/src/**: no backend crate is imported or referenced from the
#      library source (use / extern crate / path `reverie_ptrace::` etc.).
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

# Concrete Reverie backends detcore must never depend on. Extend this list if a
# new backend crate is added to the workspace.
readonly BACKEND_CRATES=(reverie-ptrace reverie-dbi reverie-kvm)
# Module-path forms (Cargo normalizes '-' to '_' for the crate identifier).
readonly BACKEND_MODS_RE='reverie_ptrace|reverie_dbi|reverie_kvm'

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
REPO_ROOT="$(cd -- "$(script_dir)/.." && pwd)"

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
    err "detcore/Cargo.toml declares a concrete Reverie backend in a non-test dependency table:"
    printf '%s\n' "$manifest_hits" >&2
    err "detcore must depend only on the abstract 'reverie' crate. Move backend wiring to hermit-cli."
    ((violations++))
else
    ok "detcore/Cargo.toml: no backend crate in [dependencies]/[build-dependencies]/[target.*]"
fi

# --- 2. library source: no backend imports -----------------------------------

src_hits="$(grep -rnE "(^|[^A-Za-z0-9_])(${BACKEND_MODS_RE})([^A-Za-z0-9_]|$)" \
    "$DETCORE_SRC" 2>/dev/null | grep -vE '^\s*[^:]+:[0-9]+:\s*//' || true)"

if [[ -n $src_hits ]]; then
    err "detcore/src references a concrete Reverie backend module:"
    printf '%s\n' "$src_hits" >&2
    err "detcore library code must use only the abstract 'reverie' interfaces."
    ((violations++))
else
    ok "detcore/src: no backend module imports (reverie_ptrace/dbi/kvm)"
fi

# --- summary -----------------------------------------------------------------

echo
if ((violations > 0)); then
    err "backend-abstraction commandment VIOLATED ($violations check(s) failed)."
    err "See detcore/src/lib.rs and detcore/Cargo.toml for the commandment."
    exit 1
fi
ok "backend-abstraction commandment intact: detcore depends only on abstract 'reverie'."
exit 0
