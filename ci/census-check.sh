#!/usr/bin/env bash
# Census gate for one counted Nextest node.
#
# Every run checks its own population: a counted node that lists no tests is
# refused (zero-tests), so an empty collection can never pass as green. That
# includes a case-selected node (--case-selected), whose filter lists only the
# cases a --tests run picked: a filter that matches nothing is refused too.
# (The counted wrapper refuses earlier, naming them, when a case validate
# --tests named is not listed.) A run
# that is given a base (the last landed main's population) also compares its
# listed population against it with ci/census-diff, so a test that vanished
# without a declared drop fails. Only a candidate-versus-main comparison needs
# that base, and ci-hub supplies it to the validates it runs. A plain run (a
# person's `make validate`, a fresh clone) has no base and needs none: it
# reports that the comparison did not run and passes, and its ledger row
# says census_checked=false, which ci-hub never reads as green evidence.
#
# Usage: ci/census-check.sh --candidate-listed FILE [--base-listed FILE]
#          [--base-dir DIR] [--candidate-outcomes FILE] [--dropped FILE]
#          [--report-only] [--case-selected] [--expected-base-commit SHA]
#          [--suite NAME]
#
# --base-dir names the adapter's per-suite supply directory
# (HERMIT_CENSUS_BASE_DIR): one stamped <suite>.listed-ids per suite.
# When no single-file --base-listed is given, the base resolves to
# <dir>/<suite>.listed-ids and faces exactly the same freshness checks;
# a directory holding no file for this suite is refused by name
# (census-base-suite-missing). A single-file base wins when both are
# given (the adapter supplies it only for single-suite selections).
#
# --case-selected names the case-selected treatment: HERMIT_NEXTEST_FILTER_EXPR
# selects individual cases, and the counted wrapper ANDs it with the node's
# own filtersets (nextest would OR separate ones), so the candidate listing
# is a subset of the node's population, never the whole of it; the
# full-population comparison does not apply
# and the check reports that by name instead of refusing for want of a
# base. The disappearance guard binds full-suite counted nodes. The
# zero-tests refusal still applies.
#
# Base contract (queue-adapter-owned, recorded here as the review point):
# the base is the .listed-ids of a full counted run at the last landed
# main commit, cached under that commit and never mutated in place. The
# producer that caches a base stamps it with a provenance header as the
# first line of the file:
#
#   # census-base: commit=<full sha of that main commit> count=<id count>
#
# Freshness is enforced here, not assumed: a supplied base without that
# header is refused
# (census-base-provenance), a header whose count disagrees with the IDs
# actually in the file is refused as truncated/short
# (census-base-truncated), and a header commit that differs from
# --expected-base-commit (the counted wrapper derives it from the
# checkout's merge-base with origin/<landing branch> -- HERMIT_LANDING_BRANCH,
# ci/landing-branches.env -- or HERMIT_CENSUS_BASE_COMMIT) is refused as
# stale (census-base-stale). --report-only is a
# manual/local debugging aid only; the counted wrapper never passes it.
#
# Exit codes: 0 pass, 1 unrecorded disappearance (census-diff), 2 usage, a
# zero-tests population, or a supplied-base refusal (stale, truncated,
# provenance, suite-missing, missing/empty base file). Validate reads every
# nonzero exit of a counted node as a product failure.
#
# With no base (neither --base-listed nor --base-dir) the comparison does not
# run: the check reports that by name and passes.
# With a base, enforcement is the default: an unrecorded disappearance
# fails naming the missing IDs unless --report-only is given (the counted
# wrapper passes it only for HERMIT_CENSUS_REPORT_ONLY=1). Additions pass;
# totals are reported, never asserted.
set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR

base_listed= base_dir= candidate_listed= candidate_outcomes= dropped= suite=nextest
report_only=0 case_selected=0
expected_base_commit=${HERMIT_CENSUS_EXPECTED_BASE_COMMIT:-${HERMIT_CENSUS_BASE_COMMIT:-}}
while (($#)); do
    case $1 in
        --base-listed) base_listed=$2; shift 2 ;;
        --base-dir) base_dir=$2; shift 2 ;;
        --candidate-listed) candidate_listed=$2; shift 2 ;;
        --candidate-outcomes) candidate_outcomes=$2; shift 2 ;;
        --dropped) dropped=$2; shift 2 ;;
        --suite) suite=$2; shift 2 ;;
        --report-only) report_only=1; shift ;;
        --case-selected) case_selected=1; shift ;;
        --expected-base-commit) expected_base_commit=$2; shift 2 ;;
        *) printf 'census-check: unknown argument %s\n' "$1" >&2; exit 2 ;;
    esac
done
if [[ -z $candidate_listed && -z $candidate_outcomes ]]; then
    printf 'census-check: --candidate-listed or --candidate-outcomes is required\n' >&2
    exit 2
fi
# Zero tests collected is a failure on every run, base or none, case-selected
# or not: nothing ran, so nothing passed.
if [[ -n $candidate_listed ]] && ! grep -qv '^[[:space:]]*\(#\|$\)' -- "$candidate_listed" 2>/dev/null; then
    printf 'census-check: zero-tests: candidate population %s lists no tests; a counted node that collects nothing is refused\n' "$candidate_listed" >&2
    exit 2
fi
if ((case_selected)); then
    printf 'census %s: case-selected counted run (HERMIT_NEXTEST_FILTER_EXPR selects cases, not the suite population); full-population base comparison not applicable; the disappearance guard binds full-suite counted nodes\n' "$suite" >&2
    exit 0
fi
# Per-suite resolution: with no single-file base, the adapter's supply
# directory holds this suite's stamped base as <suite>.listed-ids. A
# supplied directory with no file for this suite is a broken supply,
# refused by name like a supplied-but-missing single file.
if [[ -z $base_listed && -n $base_dir ]]; then
    if [[ ! -f $base_dir/$suite.listed-ids ]]; then
        printf 'census-check: census-base-suite-missing: no per-suite base %s for suite %s in HERMIT_CENSUS_BASE_DIR %s; refusing (a supplied base directory must hold every suite it arms)\n' "$base_dir/$suite.listed-ids" "$suite" "$base_dir" >&2
        exit 2
    fi
    base_listed=$base_dir/$suite.listed-ids
fi
if [[ -z $base_listed ]]; then
    printf 'census %s: no base supplied, so no vanished-test comparison: this run checked its own population; a candidate validate gets its base from ci-hub (HERMIT_CENSUS_BASE_LISTED_IDS or HERMIT_CENSUS_BASE_DIR)\n' "$suite" >&2
    exit 0
fi
# A supplied base must be real: an unreadable or empty base would make every
# candidate ID an "addition" and silently pass a wiped population. Refuse by
# name instead (enforcement is fail-closed whenever a base is provided).
if [[ ! -s $base_listed ]]; then
    printf 'census-check: base population %s is missing or empty; refusing (a supplied base must name the last landed population)\n' "$base_listed" >&2
    exit 2
fi
# Freshness: the base must prove which commit it was listed at and how
# many IDs that listing held. A supplied base with no provenance at all is
# refused; a stamped base whose declared count
# disagrees with the file is truncated/short, and one stamped with a
# different commit than expected is stale. All refusals name the file
# and the check that failed; none degrades to a pass.
base_commit= base_declared_count=
base_header=$(head -n 1 -- "$base_listed")
base_header_re='^# census-base: commit=([0-9a-f]{7,40}) count=([0-9]+)$'
if [[ $base_header =~ $base_header_re ]]; then
    base_commit=${BASH_REMATCH[1]}
    base_declared_count=${BASH_REMATCH[2]}
fi
if [[ -z $base_commit ]]; then
    printf 'census-check: census-base-provenance: base population %s has no provenance header ("# census-base: commit=<sha> count=<n>"); a supplied base must be stamped; refusing\n' "$base_listed" >&2
    exit 2
fi
if [[ -n $base_commit ]]; then
    base_actual_count=$(grep -Ev '^[[:space:]]*$|^#' -- "$base_listed" | wc -l)
    base_actual_count=${base_actual_count//[[:space:]]/}
    if [[ $base_actual_count != "$base_declared_count" ]]; then
        printf 'census-check: census-base-truncated: base population %s declares count=%s but holds %s ids; a short/truncated base is refused by name\n' "$base_listed" "$base_declared_count" "$base_actual_count" >&2
        exit 2
    fi
    if [[ -n $expected_base_commit && $base_commit != "$expected_base_commit" ]]; then
        printf 'census-check: census-base-stale: base population %s was listed at commit %s, expected base commit %s; refusing a base from the wrong commit\n' "$base_listed" "$base_commit" "$expected_base_commit" >&2
        exit 2
    fi
fi
if [[ -n $candidate_listed && ! -s $candidate_listed ]]; then
    printf 'census-check: candidate population %s is missing or empty; refusing\n' "$candidate_listed" >&2
    exit 2
fi
if [[ -n $candidate_outcomes && ! -s $candidate_outcomes ]]; then
    printf 'census-check: candidate outcomes %s are missing or empty; refusing\n' "$candidate_outcomes" >&2
    exit 2
fi
if [[ -n $dropped && ! -f $dropped ]]; then
    printf 'census-check: dropped-records file %s is missing; refusing\n' "$dropped" >&2
    exit 2
fi

# A stamped first base (census_base.py, for a suite with no prior base)
# names itself with a `# census-first-base:` marker as its second line.
# The adapter pre-creates that file in the supply directory only for a
# suite absent from the base DAG, so reaching here means the suite is
# genuinely new: the candidate's own listing becomes its first base
# when the landing gate records it, and there is no prior population
# to disappear from. Report it by name, then run the normal comparator
# below regardless: the marker explains an empty baseline, it does not
# exempt candidate or baseline integrity, so a marked baseline carrying
# IDs, duplicate candidate IDs, or malformed outcomes refuse exactly as
# they would unmarked. A suite that already existed at the base gets no
# such file from the adapter: its missing base already refused above as
# census-base-suite-missing (directory supply), never as a first base.
if grep -q '^# census-first-base:' "$base_listed"; then
    marker=$(grep -m1 '^# census-first-base:' "$base_listed")
    count=0
    if [[ -n $candidate_listed ]]; then
        count=$(grep -cv '^#' "$candidate_listed" || true)
    fi
    printf 'census %s: first base recorded from candidate (%s; candidate ids=%s; no prior base)\n' "$suite" "$marker" "$count" >&2
fi

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/base" "$scratch/candidate"
grep -Ev '^#' -- "$base_listed" >"$scratch/base/$suite.listed-ids"
if [[ -n $candidate_outcomes ]]; then
    cp -- "$candidate_outcomes" "$scratch/candidate/$suite.outcomes.json"
elif [[ -n $candidate_listed ]]; then
    cp -- "$candidate_listed" "$scratch/candidate/$suite.listed-ids"
fi
args=(--base "$scratch/base" --candidate "$scratch/candidate")
[[ -n $dropped ]] && args+=(--dropped "$dropped")
((report_only)) && args+=(--report-only)
python3 "$SCRIPT_DIR/census-diff" "${args[@]}"
