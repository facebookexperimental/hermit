#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

APPLICATION_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly APPLICATION_DIR
REPO_ROOT=$(cd -- "$APPLICATION_DIR/../../../.." && pwd)
readonly REPO_ROOT
readonly HERMIT_BIN=${HERMIT_BIN:-"$REPO_ROOT/target/debug/hermit"}
readonly HERMIT_APPLICATION_TIMEOUT=${HERMIT_APPLICATION_TIMEOUT:-120}

function require_commands {
    local command

    for command in "$@"; do
        if ! command -v "$command" >/dev/null 2>&1; then
            printf 'required application-test command not found: %s\n' "$command" >&2
            return 1
        fi
    done

    if [[ ! -x $HERMIT_BIN ]]; then
        printf 'Hermit binary not found or not executable: %s\n' "$HERMIT_BIN" >&2
        return 1
    fi
}

function assert_native_nondeterminism {
    local label=$1
    local first=$2
    local second=$3

    if [[ $first == "$second" ]]; then
        printf '%s native probes unexpectedly matched:\n%s\n' "$label" "$first" >&2
        return 1
    fi
}

# Strict L2 for an application, established from the TYPED verdict rather than
# from the exit code or a log banner.
#
# The previous implementation claimed "every application must exercise strict
# L2" and then ran bare `--verify`, which compares under the LOSSY Stripped
# policy and cannot earn L2 at all; it then decided success by grepping stderr
# for "Success: deterministic. Determinism verified.". Both halves were proxies.
# A banner is a printed marker, not a verdict, and an exit code belongs to the
# GUEST -- a guest that exits nonzero makes a passing verification look failed,
# and a guest that exits zero while its runs diverge looks passed.
#
# So this reads `--verify-json`, whose contract is documented on
# `RunOpts::verify_json` and is explicit that a parity ratchet "must key on
# `bitwise_parity`, NOT `verified`" -- `verified` is true for a stripped match
# too. `bitwise_parity` is true only under the canonical (`--verify-strict`)
# policy, and `compared_log_messages` is what makes it falsifiable: a strict
# CONFIGURATION is not evidence that the configured comparison had any data.
#
# Four outcomes are kept distinct, because collapsing them is how a harness
# reports green for work it never did:
#   REFUSED    the report was never written -- Hermit did not get far enough
#   NO-RESULT  a report exists but reached no verdict, or compared 0 messages
#   DIVERGED   a real comparison ran and the runs did not match
#   PASS       bitwise_parity AND both compared counts > 0
function run_hermit_verify {
    local label=$1
    shift

    # Because the guest runs from its private /tmp, callers must explicitly name
    # any guest-argv positions that carry host-resolved paths. Checking whether
    # an arbitrary token happens to exist in the host cwd would make admission
    # depend on unrelated directory contents, and it would miss paths that do
    # not exist yet. Positions are one-based and are checked lexically, before
    # the guest is launched:
    #
    #   run_hermit_verify label --require-absolute-arg 1 \
    #       --require-absolute-arg 2 -- /bin/bash /abs/script
    local -a required_absolute_args=()
    while (($#)); do
        case $1 in
            --require-absolute-arg)
                if (($# < 2)) || [[ ! $2 =~ ^[1-9][0-9]*$ ]]; then
                    printf '%s: PATH-CONTRACT: --require-absolute-arg needs a positive guest argument position\n' \
                        "$label" >&2
                    return 1
                fi
                required_absolute_args+=("$2")
                shift 2
                ;;
            --)
                shift
                break
                ;;
            *)
                printf '%s: PATH-CONTRACT: expected helper options followed by --, got %s\n' \
                    "$label" "$1" >&2
                return 1
                ;;
        esac
    done

    local -a guest_argv=("$@")
    if ((${#guest_argv[@]} == 0)); then
        printf '%s: PATH-CONTRACT: missing guest command after --\n' "$label" >&2
        return 1
    fi

    local position index path_arg
    for position in "${required_absolute_args[@]}"; do
        index=$((position - 1))
        if ((index >= ${#guest_argv[@]})); then
            printf '%s: PATH-CONTRACT: guest argument position %s is not present\n' \
                "$label" "$position" >&2
            return 1
        fi
        path_arg=${guest_argv[index]}
        if [[ $path_arg != /* ]]; then
            printf '%s: PATH-CONTRACT: guest argument %s must be absolute because the guest cwd is /tmp: %s\n' \
                "$label" "$position" "$path_arg" >&2
            return 1
        fi
    done

    local stdout_file stderr_file verdict_file status=0
    stdout_file=$(mktemp "${TMPDIR:-/tmp}/hermit-app-stdout.XXXXXX")
    stderr_file=$(mktemp "${TMPDIR:-/tmp}/hermit-app-stderr.XXXXXX")
    verdict_file=$(mktemp "${TMPDIR:-/tmp}/hermit-app-verdict.XXXXXX")
    # Hermit writes the report itself; an existing empty file must not be
    # mistaken for a report it produced.
    rm -f -- "$verdict_file"

    # `--workdir=/tmp` makes the guest's working directory HERMETIC, and that is
    # a correctness requirement of the comparison, not a convenience.
    #
    # Without it the guest inherits this harness's cwd. During a full local
    # validate that cwd is the throwaway checkout under the parent workspace's
    # shared `ignored/` tree, which a dozen concurrent agents write to. Bash
    # validates an inherited `$PWD` at startup by stat()ing EVERY ancestor
    # component of it, so any nested shell the guest spawns -- including the one
    # behind a `/usr/local/bin/*` wrapper script -- stats those shared
    # directories. On btrfs a directory's `st_size` tracks its entries, so a
    # sibling agent creating one file between run 1 and run 2 changes that
    # st_size and the `--verify` pair diverges on a real, correctly-reported
    # difference. Hermit does not make a changing filesystem deterministic; it
    # is the test's job to present a stable one.
    #
    # `/tmp` is the right target because Hermit bind-mounts a fresh private
    # directory over it, and `--workdir` is resolved AFTER guest mounts are
    # applied, so this names the guest's isolated view. Both properties matter:
    # merely `cd`ing to /tmp before launching would bind the cwd to the
    # *shadowed host* /tmp (62k entries on this box, churning constantly).
    #
    # The guest's whole ancestor chain is then `/` plus its own private /tmp,
    # and every mutation under it is the guest's own -- which Hermit already
    # makes deterministic.
    timeout "$HERMIT_APPLICATION_TIMEOUT" \
        "$HERMIT_BIN" --log=info run --no-virtualize-cpuid \
        --max-timeslice=disabled --base-env=minimal --strict --workdir=/tmp \
        --verify --verify-strict --verify-json "$verdict_file" -- \
        "${guest_argv[@]}" >"$stdout_file" 2>"$stderr_file" || status=$?

    local failure=""
    if [[ ! -s $verdict_file ]]; then
        # No typed verdict at all. Hermit stamps a no-result report BEFORE the
        # runs are compared, so an absent/empty file means it never reached even
        # that point: a launch refusal, not a comparison outcome.
        failure="REFUSED: no --verify-json report was written (Hermit exited $status before stamping a verdict)"
    else
        local parity counts_left counts_right verdict_name
        parity=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("bitwise_parity"))' "$verdict_file" 2>/dev/null || echo ERROR)
        verdict_name=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("verdict"))' "$verdict_file" 2>/dev/null || echo ERROR)
        counts_left=$(python3 -c 'import json,sys; c=json.load(open(sys.argv[1])).get("compared_log_messages") or {}; print(c.get("left",0))' "$verdict_file" 2>/dev/null || echo ERROR)
        counts_right=$(python3 -c 'import json,sys; c=json.load(open(sys.argv[1])).get("compared_log_messages") or {}; print(c.get("right",0))' "$verdict_file" 2>/dev/null || echo ERROR)

        if [[ $parity == ERROR || $verdict_name == ERROR ]]; then
            failure="NO-RESULT: --verify-json report is not parseable JSON"
        elif [[ $verdict_name == no_result ]]; then
            failure="NO-RESULT: verification reached no verdict (verdict=no_result)"
        elif [[ $counts_left == 0 || $counts_right == 0 ]]; then
            # A strict configuration that compared nothing is not parity. This is
            # the check that keeps a vacuously-matching selection from reading as
            # L2 -- the same rule as "test result: ok with zero executed tests".
            failure="NO-RESULT: compared 0 log messages (left=$counts_left right=$counts_right); a strict configuration is not evidence the comparison had data"
        elif [[ $parity != True ]]; then
            failure="DIVERGED: bitwise_parity=$parity verdict=$verdict_name (a stripped match does NOT earn L2)"
        fi
    fi

    if [[ -n $failure ]]; then
        printf '%s did not earn strict L2 -- %s\nverdict json:\n' "$label" "$failure" >&2
        # Order matters: `cat f 2>/dev/null >&2` would point stdout at the
        # ALREADY-redirected fd 2 (/dev/null) and silently discard the report --
        # the exact class of "evidence that isn't there" this helper exists to
        # stop. Redirect stdout to stderr FIRST.
        if [[ -s $verdict_file ]]; then cat "$verdict_file" >&2; else printf '(absent)\n' >&2; fi
        printf '\nstdout:\n' >&2
        cat "$stdout_file" >&2
        printf 'stderr:\n' >&2
        cat "$stderr_file" >&2
        rm -f -- "$stdout_file" "$stderr_file" "$verdict_file"
        return 1
    fi

    # The guest's own exit code is checked only AFTER the typed verdict, and is
    # reported as its own condition rather than being conflated with parity.
    if ((status != 0)); then
        printf '%s: strict L2 parity held, but the guest exited nonzero (status %s)\nstdout:\n' "$label" "$status" >&2
        cat "$stdout_file" >&2
        printf 'stderr:\n' >&2
        cat "$stderr_file" >&2
        rm -f -- "$stdout_file" "$stderr_file" "$verdict_file"
        return "$status"
    fi

    cat "$stdout_file"
    rm -f -- "$stdout_file" "$stderr_file" "$verdict_file"
}
