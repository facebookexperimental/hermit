#!/usr/bin/env bash
# Census gate for one counted Nextest node: compare this run's listed
# population against a base (last landed main) population with ci/census-diff.
#
# Usage: ci/census-check.sh --candidate-listed FILE [--base-listed FILE]
#          [--candidate-outcomes FILE] [--dropped FILE] [--report-only]
#          [--require-base] [--allow-no-base] [--suite NAME]
#
# Base contract (queue-adapter-owned, recorded here as the review point):
# the base is the .listed-ids of a full counted run at the last landed
# main commit, cached under that commit and never mutated in place. A
# base cast from an older or partial run is stale: this gate cannot tell
# a short base from a complete one, so freshness is the adapter's duty
# and a stale base is an adapter bug, not a pass. --report-only is a
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

base_listed= candidate_listed= candidate_outcomes= dropped= suite=nextest
report_only=0 require_base=0 allow_no_base=0
while (($#)); do
    case $1 in
        --base-listed) base_listed=$2; shift 2 ;;
        --candidate-listed) candidate_listed=$2; shift 2 ;;
        --candidate-outcomes) candidate_outcomes=$2; shift 2 ;;
        --dropped) dropped=$2; shift 2 ;;
        --suite) suite=$2; shift 2 ;;
        --report-only) report_only=1; shift ;;
        --require-base) require_base=1; shift ;;
        --allow-no-base) allow_no_base=1; shift ;;
        *) printf 'census-check: unknown argument %s\n' "$1" >&2; exit 2 ;;
    esac
done
if [[ -z $candidate_listed && -z $candidate_outcomes ]]; then
    printf 'census-check: --candidate-listed or --candidate-outcomes is required\n' >&2
    exit 2
fi
if [[ -z $base_listed ]]; then
    if ((require_base && ! allow_no_base)); then
        printf 'census-check: no base population supplied and --require-base is in force; the queue adapter must provide HERMIT_CENSUS_BASE_LISTED_IDS (local runs opt out explicitly with HERMIT_CENSUS_ALLOW_NO_BASE=1)\n' >&2
        exit 2
    fi
    printf 'census %s: report-only (no base population supplied; queue adapter has not provided HERMIT_CENSUS_BASE_LISTED_IDS)\n' "$suite" >&2
    exit 0
fi
# A supplied base must be real: an unreadable or empty base would make every
# candidate ID an "addition" and silently pass a wiped population. Refuse by
# name instead (enforcement is fail-closed whenever a base is provided).
if [[ ! -s $base_listed ]]; then
    printf 'census-check: base population %s is missing or empty; refusing (a supplied base must name the last landed population)\n' "$base_listed" >&2
    exit 2
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
cp -- "$base_listed" "$scratch/base/$suite.listed-ids"
if [[ -n $candidate_outcomes ]]; then
    cp -- "$candidate_outcomes" "$scratch/candidate/$suite.outcomes.json"
elif [[ -n $candidate_listed ]]; then
    cp -- "$candidate_listed" "$scratch/candidate/$suite.listed-ids"
fi
args=(--base "$scratch/base" --candidate "$scratch/candidate")
[[ -n $dropped ]] && args+=(--dropped "$dropped")
((report_only)) && args+=(--report-only)
python3 "$SCRIPT_DIR/census-diff" "${args[@]}"
