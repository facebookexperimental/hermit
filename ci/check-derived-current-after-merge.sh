#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# check-derived-current-after-merge.sh — assert the DERIVED compatibility
# artifacts are current for the tree that will exist AFTER this branch merges,
# not merely for the tree the author built on.
#
# WHY THE "AFTER MERGE" PART IS THE WHOLE POINT
#
# SCORECARD.md and ci/compat-envelope/cells.json are derived from
# ci/expected-e2e-plan.json and tests/e2e/manifests/*.yaml. Nothing re-runs the
# generator when an input changes, and gate.manifest only notices once the commit
# is ON MAIN -- where it is the first blocking node and truncates every
# subsequent validate at 4 nodes. Four commits after hermit#2482 touched
# derivation inputs and regenerated nothing; three escaped only because their
# edits did not happen to move the derived output.
#
# An author who runs `scorecard.rs check` before pushing answers
#     "were the artifacts regenerated at MY base?"
# The question that actually matters is
#     "will the artifacts be current once this lands on the CURRENT main?"
# Those are the same question only while no one else touches a derivation input.
# They diverge exactly when two branches both do -- which is the case that
# produced tonight's stall.
#
# MEASURED, so the limits of this check are on the record (2026-08-25):
#   * Two branches editing DIFFERENT manifests in disjoint regions each passed at
#     their own base, merged with no conflict, AND the merged tree still passed.
#     For that shape, base-checking was already sufficient; this check agrees
#     with it and costs one merge.
#   * The divergence therefore needs a shared region or an aggregate field (the
#     totals line in SCORECARD.md is the obvious one). This check catches that
#     shape because it regenerates against the merged inputs rather than either
#     side's.
#
# ⚠️ STATUS: ADVISORY, NOT A GATE, AND DELIBERATELY SO.
#
# Its marginal value over simply running `scorecard.rs check` on the branch is
# UNPROVEN. I could not construct a case where the two disagree. Attempted twice:
#   * two branches editing DIFFERENT manifests, each regenerated at its own base,
#     merged with no conflict -> merged tree still CURRENT;
#   * a branch regenerated at its base, merged onto a main that had since landed
#     its OWN input change -> merged tree still CURRENT.
# In both, git's textual merge of the derived files produced the correct result
# because the edits fell in disjoint regions.
#
# THE OPEN QUESTION, left here so the next person finds it instead of re-deriving
# it: the divergence should require a SHARED region or an AGGREGATE field -- the
# totals line in SCORECARD.md is the obvious candidate, since two branches that
# each add a cell both rewrite it and git must pick one. I could not build that
# case; flipping a `ci: false` to `true` to move the totals hit a manifest
# validity error (`naked ci=true requires at least one enabled backend`) before it
# could produce a staleness.
#
# DEMONSTRATE THAT DISAGREEMENT AND THIS EARNS PROMOTION TO A GATE. Until then it
# stays advisory: wiring a blocking check whose benefit has not been measured is
# how the next genuinely necessary gate gets ignored.
#
# CONDITIONAL BY DESIGN. It exits 0 immediately when the branch touches no
# derivation input. A check that runs on every commit gets resented and then
# bypassed, and an unenforced check is worse than none -- it reads as coverage
# while providing none.
#
# Usage:  ci/check-derived-current-after-merge.sh [BASE_REF]     (default origin/main)
# Exit:   0 not applicable, or derived artifacts current after merge
#         1 could not evaluate (dirty tree, merge conflict) -- fail closed, not silent
#         2 derived artifacts are STALE after merge; run scorecard.rs update

set -euo pipefail

BASE_REF="${1:-origin/main}"
GENERATOR="ci/compat-envelope/scorecard.rs"

INPUT_GLOBS=(
    'ci/expected-e2e-plan.json'
    'tests/e2e/manifests/*.yaml'
)

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

git rev-parse --verify --quiet "$BASE_REF" >/dev/null || {
    echo "check-derived-current-after-merge: base ref '$BASE_REF' does not resolve" >&2
    exit 1
}

merge_base=$(git merge-base "$BASE_REF" HEAD)
mapfile -t changed < <(git diff --name-only "$merge_base" HEAD)

touched=()
for path in "${changed[@]}"; do
    for glob in "${INPUT_GLOBS[@]}"; do
        # shellcheck disable=SC2053 # intentional glob match, not string equality
        if [[ $path == $glob ]]; then touched+=("$path"); fi
    done
done

if [[ ${#touched[@]} -eq 0 ]]; then
    echo "check-derived-current-after-merge: no derivation input touched; not applicable"
    exit 0
fi

echo "check-derived-current-after-merge: derivation inputs touched:"
printf '    %s\n' "${touched[@]}"

if [[ -n $(git status --porcelain) ]]; then
    echo "check-derived-current-after-merge: working tree is dirty; refusing rather than" >&2
    echo "  evaluating a tree that is neither the branch nor the merge result" >&2
    exit 1
fi

# Resolve HEAD to a SHA in THIS repo before handing it to the scratch worktree.
# `git -C "$worktree" merge HEAD` resolves HEAD *inside that worktree*, where it is
# the base commit -- so the merge is a no-op and the script silently checks the BASE
# instead of the merge result. That defect made this script report the base's status
# for every branch; it was caught only because a correctly-regenerated branch was
# refused. Pass the SHA, never the symbolic name.
head_sha=$(git rev-parse HEAD)

parent_root=${DEV_HERMIT_PARENT:-$(git rev-parse --show-superproject-working-tree 2>/dev/null || true)}
wrkslots="$parent_root/ci-hub/bin/wrkslots"
if [[ -z $parent_root || ! -x $wrkslots ]]; then
    echo "check-derived-current-after-merge: wrkslots is unavailable." >&2
    echo "  state: REFUSED -- the merged tree was not created and the generator never ran." >&2
    echo "  remedy: run from a dev-hermit slot with DEV_HERMIT_PARENT set, or ask the coordinator to provide one." >&2
    exit 1
fi
slot="derived-${head_sha:0:12}-$$"
slot_created=0
worktree=""
# shellcheck disable=SC2317 # invoked indirectly by the trap below
cleanup() {
    local rc=$?
    trap - EXIT
    if [[ $slot_created -eq 1 ]]; then
        cleanup_output=$(
            "$wrkslots" --allow-existing-unregistered-worktrees \
                remove "$slot" --validate-complete \
                --coordinator-pid "$$" --expected-generation 1 2>&1
        )
        cleanup_rc=$?
        if [[ $cleanup_rc -ne 0 ]]; then
            printf '%s\n' \
                "check-derived-current-after-merge: validation checkout RETAINED after wrkslots cleanup refused." \
                "  state: RETAINED -- the check exit remains $rc; cleanup did not complete." \
                "  remedy: $cleanup_output" >&2
        fi
    fi
    exit "$rc"
}
trap cleanup EXIT

create_output=$(
    "$wrkslots" --allow-existing-unregistered-worktrees \
        create "$slot" --format json --slot-type validate \
        --coordinator-authorized --agent "derived-check-$$" \
        --task "derived-current-after-merge" \
        --purpose "evaluate derived artifacts in the prospective merged tree" \
        --owner-pid "$$" --coordinator-pid "$$" \
        --repo checkout=hermit --start "checkout=$BASE_REF"
)
create_rc=$?
if [[ $create_rc -ne 0 ]]; then
    echo "check-derived-current-after-merge: wrkslots refused the validation checkout." >&2
    echo "  state: REFUSED -- the merged tree was not created and the generator never ran." >&2
    echo "  remedy: follow the wrkslots refusal above, then rerun this check." >&2
    exit 1
fi
slot_created=1
worktree=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["checkouts"][0]["path"])' <<<"$create_output")
parse_rc=$?
if [[ $parse_rc -ne 0 || ! -d $worktree ]]; then
    echo "check-derived-current-after-merge: wrkslots returned no readable checkout path." >&2
    echo "  state: REFUSED -- the generator never ran." >&2
    echo "  remedy: run '$wrkslots status --slot $slot --format json' and inspect the recorded path." >&2
    exit 1
fi

if ! git -C "$worktree" -c core.hooksPath=/dev/null merge --no-edit --no-ff "$head_sha" >/dev/null 2>&1; then
    echo "check-derived-current-after-merge: HEAD does not merge cleanly onto $BASE_REF." >&2
    echo "  Rebase first; the derived artifacts cannot be judged against a tree that" >&2
    echo "  does not exist." >&2
    exit 1
fi

echo "check-derived-current-after-merge: evaluating the MERGED tree, not the branch head"
# Run the generator IN the merged worktree. Note this is a plain subshell cd, not
# `git -C`: the generator is not a git subcommand, and invoking it that way fails
# with "is not a git command" -- which this script would then have reported as
# STALE, refusing every applicable branch for the wrong reason. Caught in test.
if (cd "$worktree" && "./$GENERATOR" check); then
    echo "check-derived-current-after-merge: derived artifacts are current after merge"
    exit 0
fi

cat >&2 <<EOF

check-derived-current-after-merge: DERIVED ARTIFACTS ARE STALE AFTER MERGE.

  They may well be current at your own base -- that is not the question. Once
  this branch lands on $BASE_REF the tracked SCORECARD.md and
  ci/compat-envelope/cells.json will not match what the generator derives from
  the merged inputs, and gate.manifest will fail on main as the first blocking
  node, truncating every validate behind it.

  Fix, from your branch rebased onto $BASE_REF:

      ./$GENERATOR update
      ./$GENERATOR check
      git commit SCORECARD.md ci/compat-envelope/cells.json

EOF
exit 2
