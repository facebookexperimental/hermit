#!/usr/bin/env bash
#
# Prepare the pinned hermit_test_ledger objects the scorecard self-test reads.
#
# Usage: ci/prepare-scorecard-self-test-corpus.sh PARENT_DIR
#
# ci/compat-envelope/scorecard.rs retains its complete historical self-test
# corpus in the existing public ledger (SELF_TEST_CORPUS). The test refuses
# missing data and never fetches, so every hosted job that runs the self-test
# must prepare these objects first and export DEV_HERMIT_TEST_LEDGER_ROOT.
# Hosted, `selftest.scorecard_commands` and `check.script_unit_tests` (via
# scripts/run-script-tests.sh) both read it in the checks job; the regression
# tier in `selftest.scorecard` reads no ledger. Run 36485831200
# prepared the corpus only in the preflight job, whose gate.manifest ran the
# self-test until 2026-09-29, so the checks job failed the corpus test there.
#
# The script creates a fresh directory under PARENT_DIR, fetches only the pinned
# commit's tree and the two pinned blobs, and prints the directory on stdout only
# after every object has been verified locally without lazy fetching.
set -euo pipefail
# Git exports its repository-location variables to hooks and `git rebase
# --exec` steps, and they override `git -C`. Every repository below is named
# explicitly, so run without them; otherwise a scratch `git init` rewrites the
# caller's repository (https://github.com/rrnewton/hermit/issues/3362).
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY \
    GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_PREFIX

if [ "$#" -ne 1 ] || [ ! -d "$1" ]; then
    echo "usage: $0 PARENT_DIR (an existing directory)" >&2
    exit 2
fi

corpus_commit=352c54f800e62426fe54bd58c9110e8d93d8daaf
archive_blob=c55b6e644a6f32aadfdea8fbbf76dfaddb20de3e
identity_blob=cc25483da6bf92e361a0a7596ba87ea33872eb15

# These pins duplicate SELF_TEST_CORPUS in scorecard.rs. Refuse when they drift
# instead of preparing a corpus the test would then reject.
scorecard="$(dirname "$0")/compat-envelope/scorecard.rs"
for pin in "ledger_commit: \"$corpus_commit\"" \
    "archive_blob: \"$archive_blob\"" \
    "identity_blob: \"$identity_blob\""; do
    if ! grep -qF -- "$pin" "$scorecard"; then
        echo "error: $scorecard no longer pins $pin; update $0 to match SELF_TEST_CORPUS" >&2
        exit 1
    fi
done

corpus_root="$(mktemp -d "$1/scorecard-self-test-corpus.XXXXXX")"
timeout --foreground --kill-after=10s 120s bash -euo pipefail -s -- \
    "$corpus_root" "$corpus_commit" "$archive_blob" "$identity_blob" <<'CORPUS'
corpus_root=$1
corpus_commit=$2
shift 2
git init --quiet "$corpus_root"
git -C "$corpus_root" remote add origin https://github.com/rrnewton/hermit_test_ledger.git
git -C "$corpus_root" config remote.origin.promisor true
git -C "$corpus_root" config remote.origin.partialclonefilter blob:none
git -C "$corpus_root" -c protocol.version=2 fetch --quiet --no-tags --depth=1 --filter=blob:none origin "$corpus_commit"
# Fetch only the two pinned fixture blobs before the no-fetch test begins.
for object in "$@"; do
  git -C "$corpus_root" cat-file -e "$object^{blob}" >&2
  GIT_NO_LAZY_FETCH=1 git -C "$corpus_root" cat-file -e "$object^{blob}" >&2
done
CORPUS
printf '%s\n' "$corpus_root"
