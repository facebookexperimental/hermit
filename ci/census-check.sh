#!/usr/bin/env bash
# Census gate for one counted Nextest node: compare this run's listed
# population against a base (last landed main) population with ci/census-diff.
#
# Usage: ci/census-check.sh --candidate-listed FILE [--base-listed FILE]
#          [--base-dir DIR] [--candidate-outcomes FILE] [--dropped FILE]
#          [--report-only] [--require-base] [--allow-no-base]
#          [--case-selected] [--expected-base-commit SHA] [--suite NAME]
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
# selects individual cases, so the candidate listing is a subset, never
# the suite population; the full-population comparison does not apply
# and the check reports that by name instead of refusing for want of a
# base. The disappearance guard binds full-suite counted nodes.
#
# Base contract (queue-adapter-owned, recorded here as the review point):
# the base is the .listed-ids of a full counted run at the last landed
# main commit, cached under that commit and never mutated in place. The
# producer that caches a base stamps it with a provenance header as the
# first line of the file:
#
#   # census-base: commit=<full sha of that main commit> count=<id count>
#
# Freshness is enforced here, not assumed: with --require-base (every
# counted node) a base without that header is refused
# (census-base-provenance), a header whose count disagrees with the IDs
# actually in the file is refused as truncated/short
# (census-base-truncated), and a header commit that differs from
# --expected-base-commit (the counted wrapper derives it from the
# checkout's merge-base with origin/main, or HERMIT_CENSUS_BASE_COMMIT)
# is refused as stale (census-base-stale). --report-only is a
# manual/local debugging aid only; the counted wrapper never passes it.
#
# With no --base-listed there is no base population in-node (the queue
# adapter owns the cached base; see HERMIT_CENSUS_BASE_LISTED_IDS in
# ci/run-nextest-counted.sh): the check reports that and passes, so wiring
# it into every counted node is safe before the adapter supplies a base.
# With a base, enforcement is the default: an unrecorded disappearance
# fails naming the missing IDs unless --report-only is given (the counted
# wrapper passes it only for HERMIT_CENSUS_REPORT_ONLY=1). Additions pass;
# totals are reported, never asserted.
set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR

base_listed= base_dir= candidate_listed= candidate_outcomes= dropped= suite=nextest
report_only=0 require_base=0 allow_no_base=0 case_selected=0
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
        --require-base) require_base=1; shift ;;
        --allow-no-base) allow_no_base=1; shift ;;
        --case-selected) case_selected=1; shift ;;
        --expected-base-commit) expected_base_commit=$2; shift 2 ;;
        *) printf 'census-check: unknown argument %s\n' "$1" >&2; exit 2 ;;
    esac
done
if [[ -z $candidate_listed && -z $candidate_outcomes ]]; then
    printf 'census-check: --candidate-listed or --candidate-outcomes is required\n' >&2
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
    if ((require_base && ! allow_no_base)); then
        printf 'census-check: no base population supplied and --require-base is in force; the queue adapter must provide HERMIT_CENSUS_BASE_LISTED_IDS or HERMIT_CENSUS_BASE_DIR (local runs opt out explicitly with HERMIT_CENSUS_ALLOW_NO_BASE=1)\n' >&2
        exit 2
    fi
    printf 'census %s: report-only (no base population supplied; queue adapter has not provided HERMIT_CENSUS_BASE_LISTED_IDS or HERMIT_CENSUS_BASE_DIR)\n' "$suite" >&2
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
# many IDs that listing held. A counted node (--require-base) refuses a
# base with no provenance at all; any stamped base whose declared count
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
if ((require_base)) && [[ -z $base_commit ]]; then
    printf 'census-check: census-base-provenance: base population %s has no provenance header ("# census-base: commit=<sha> count=<n>"); counted nodes require a stamped base; refusing\n' "$base_listed" >&2
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
