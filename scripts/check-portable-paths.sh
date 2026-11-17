#!/usr/bin/env bash
# Reject developer-specific homes and hostnames in tracked build/run files.
#
# A line that must keep a match goes in scripts/portable-paths-allowlist.txt
# with a reason: a detector's own needle is the case it exists for. An entry
# that no longer matches anything fails the check, so the list cannot silently
# outlive its reasons.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT_DIR
readonly ALLOWLIST_REL=scripts/portable-paths-allowlist.txt

is_build_or_run_file() {
    case "$1" in
        *.sh | *.bash | *.rs | *.py | *.toml | *.yml | *.yaml | *.config \
            | *.conf | *.mk | Makefile | Makefile.* | */Makefile | */Makefile.* \
            | .github/* | ci/*)
            return 0 ;;
        *) return 1 ;;
    esac
}

is_excluded() {
    [[ $1 == ci/compat-envelope/cells.json ]] && return 0
    # These are byte-authenticated historical parser inputs copied from the
    # private parent, not commands or live configuration. Their synthetic paths
    # must remain unchanged or their companion hashes stop authenticating them.
    [[ $1 == ci/manifest-plan/src/ledger/schema10/fixtures/legacy/* ]] && return 0
    case "/$1/" in
        */.git/* | */ignored/* | */experiments/* | */scratch/* | */target/* \
            | */third-party/* | */vendor/* | */scripts/check-portable-paths.sh/)
            return 0 ;;
        *) return 1 ;;
    esac
}

scan_file() {
    local path=$1
    awk '
        {
            probe = tolower($0)
            gsub(/\/(home|users)\/(user|test|example)([^[:alnum:]_.-]|$)/,
                 "/generic/", probe)
            if (probe ~ /\/(home|users)\/[[:alnum:]_.-]+([^[:alnum:]_.-]|$)/ ||
                probe ~ /(^|[^[:alnum:]_])newton([^[:alnum:]_]|$)/ ||
                probe ~ /devbig[[:alnum:]._-]*/) {
                print FNR ":" $0
                found = 1
            }
        }
        END { exit found ? 1 : 0 }
    ' "$path"
}

# Print every hit in the tracked files as "path:line:content" and exit 0,
# whether or not anything matched. Exit 2 when the tracked files cannot be
# listed: a scan of nothing is not a pass.
scan_repository() {
    local hit_file
    local listing
    local path
    hit_file=$(mktemp "$ROOT_DIR/target/portable-path-hit.XXXXXX")
    listing=$(mktemp "$ROOT_DIR/target/portable-path-files.XXXXXX")
    if ! git -C "$ROOT_DIR" ls-files -z >"$listing"; then
        echo "portability check: cannot list the tracked files of $ROOT_DIR; scanning nothing is not a pass" >&2
        rm -f "$hit_file" "$listing"
        return 2
    fi
    while IFS= read -r -d '' path; do
        is_excluded "$path" && continue
        [[ -f $ROOT_DIR/$path ]] || continue
        is_build_or_run_file "$path" || [[ -x $ROOT_DIR/$path ]] || continue
        if ! scan_file "$ROOT_DIR/$path" >"$hit_file"; then
            while IFS= read -r hit; do
                printf '%s:%s\n' "$path" "$hit"
            done <"$hit_file"
        fi
    done <"$listing"
    rm -f "$hit_file" "$listing"
}

# Read "path:line:content" hits on stdin and drop the ones the allowlist file $1
# covers: an entry "PATH | SUBSTRING | REASON" covers a hit in exactly that file
# whose line contains SUBSTRING. Print the remaining hits, then any unreadable,
# malformed or stale allowlist entry as "ALLOWLIST:<n>: ...". Exit 1 when
# anything was printed.
apply_allowlist() {
    local allowlist=$1
    if [[ ! -r $allowlist ]]; then
        echo "ALLOWLIST: cannot read $allowlist"
        return 1
    fi
    awk -v allowlist="$allowlist" '
        function trim(s) { gsub(/^[ \t]+|[ \t]+$/, "", s); return s }
        BEGIN {
            n = 0
            bad = 0
            while ((getline entry < allowlist) > 0) {
                lineno++
                if (entry ~ /^[ \t]*(#|$)/) continue
                count = split(entry, field, / \| /)
                if (count != 3 || trim(field[1]) == "" || trim(field[2]) == "" ||
                    trim(field[3]) == "") {
                    print "ALLOWLIST:" lineno ": malformed entry (need PATH | SUBSTRING | REASON): " entry
                    bad = 1
                    continue
                }
                n++
                entry_path[n] = trim(field[1])
                entry_text[n] = trim(field[2])
                entry_line[n] = lineno
                used[n] = 0
            }
            close(allowlist)
        }
        {
            path = $0
            sub(/:.*/, "", path)
            content = substr($0, length(path) + 2)
            sub(/^[0-9]+:/, "", content)
            allowed = 0
            for (i = 1; i <= n; i++) {
                if (path == entry_path[i] && index(content, entry_text[i]) > 0) {
                    used[i] = 1
                    allowed = 1
                }
            }
            if (!allowed) {
                print
                bad = 1
            }
        }
        END {
            for (i = 1; i <= n; i++) {
                if (!used[i]) {
                    print "ALLOWLIST:" entry_line[i] ": stale entry, it matches no hit: " entry_path[i] " | " entry_text[i]
                    bad = 1
                }
            }
            exit bad
        }
    '
}

self_test() {
    local fixture
    fixture=$(mktemp)

    printf '%s\n' "cache_dir=\"\${HOME}/.cache/hermit\"" >"$fixture"
    scan_file "$fixture" >/dev/null || {
        echo "portability self-test rejected a HOME-relative path" >&2
        rm -f "$fixture"
        return 1
    }

    printf 'cache_dir="/home/ci-portability-owner/.cache/hermit"\n' >"$fixture"
    if scan_file "$fixture" >/dev/null; then
        echo "portability self-test failed to reject a literal developer home" >&2
        rm -f "$fixture"
        return 1
    fi

    printf '%s\n' '# Measured on devbig-portability-test' >"$fixture"
    if scan_file "$fixture" >/dev/null; then
        echo "portability self-test failed to reject a literal measurement host" >&2
        rm -f "$fixture"
        return 1
    fi

    printf '%s\n' '# Measured on the host recorded in docs/TESTING_ENVIRONMENTS.md under "Named measurement hosts"' >"$fixture"
    scan_file "$fixture" >/dev/null || {
        echo "portability self-test rejected the documented measurement-host reference" >&2
        rm -f "$fixture"
        return 1
    }

    is_excluded ci/compat-envelope/cells.json || {
        echo "portability self-test failed to exclude literal compatibility evidence" >&2
        rm -f "$fixture"
        return 1
    }
    is_excluded ci/manifest-plan/src/ledger/schema10/fixtures/legacy/reference-diverged-cells.jsonl || {
        echo "portability self-test failed to exclude authenticated legacy evidence" >&2
        rm -f "$fixture"
        return 1
    }
    if is_excluded ci/manifest-plan/src/ledger/schema10/tests.rs; then
        echo "portability self-test widened the legacy-evidence exclusion to live parser code" >&2
        rm -f "$fixture"
        return 1
    fi
    if is_excluded ci/manifest-plan/src/ledger/schema10/fixtures/current/probe.jsonl; then
        echo "portability self-test widened the legacy-evidence exclusion to current fixtures" >&2
        rm -f "$fixture"
        return 1
    fi
    if is_excluded ci/compat-envelope/scorecard.rs; then
        echo "portability self-test excluded live compatibility code" >&2
        rm -f "$fixture"
        return 1
    fi
    if is_excluded archived/ci/compat-envelope/cells.json; then
        echo "portability self-test widened the literal evidence exclusion" >&2
        rm -f "$fixture"
        return 1
    fi
    if is_build_or_run_file fixtures/evidence.txt; then
        echo "portability self-test unexpectedly scans arbitrary text evidence" >&2
        rm -f "$fixture"
        return 1
    fi

    # ⚠️ WHERE A HOST IDENTITY IS SUPPOSED TO GO, DECLARED RATHER THAN INFERRED.
    # Removing a hostname from a build/run file destroys the provenance of whatever
    # was measured on it, and a measurement whose host is unrecorded cannot be re-run
    # or challenged. The designated destination is the "Named measurement hosts"
    # section of docs/TESTING_ENVIRONMENTS.md. That file is out of scope because it
    # neither builds nor runs -- NOT because the scanner happens to miss it. The two
    # assertions below pin that distinction: the destination must stay out of scope,
    # and the predicate must still say yes to something, so a uniformly-false
    # predicate cannot pass this test.
    if is_build_or_run_file docs/TESTING_ENVIRONMENTS.md; then
        echo "portability self-test: the designated host-provenance file is in scope; \
move the identities or update the designation" >&2
        rm -f "$fixture"
        return 1
    fi
    is_build_or_run_file ci/dag/validate.json || {
        echo "portability self-test: scope predicate said no to a file it must scan" >&2
        rm -f "$fixture"
        return 1
    }

    # The allowlist: an entry hides only lines of its own file that contain its
    # text, a fully covered scan passes, and a stale or malformed entry fails.
    local out rc
    printf '%s\n' '# comment' 'a.rs | "needle" | functional' >"$fixture"
    out=$(printf '%s\n' 'a.rs:3:    find("needle")' 'b.rs:4:    find("needle")' \
        | apply_allowlist "$fixture") && rc=0 || rc=$?
    if [[ $rc -ne 1 || $out != 'b.rs:4:    find("needle")' ]]; then
        echo "portability self-test: allowlist filtering is wrong (rc=$rc): $out" >&2
        rm -f "$fixture"
        return 1
    fi
    out=$(printf '%s\n' 'a.rs:3:    find("needle")' | apply_allowlist "$fixture") && rc=0 || rc=$?
    if [[ $rc -ne 0 || -n $out ]]; then
        echo "portability self-test: a fully allowlisted scan did not pass (rc=$rc): $out" >&2
        rm -f "$fixture"
        return 1
    fi
    out=$(printf '' | apply_allowlist "$fixture") && rc=0 || rc=$?
    if [[ $rc -ne 1 || $out != *'stale entry'*'a.rs'* ]]; then
        echo "portability self-test: a stale allowlist entry was not reported (rc=$rc): $out" >&2
        rm -f "$fixture"
        return 1
    fi
    printf '%s\n' 'a.rs | "needle" |' >"$fixture"
    out=$(printf '%s\n' 'a.rs:3:    find("needle")' | apply_allowlist "$fixture") && rc=0 || rc=$?
    if [[ $rc -ne 1 || $out != *'malformed entry'* ]]; then
        echo "portability self-test: an entry without a reason was accepted (rc=$rc): $out" >&2
        rm -f "$fixture"
        return 1
    fi
    rm -f "$fixture"
}

mkdir -p "$ROOT_DIR/target"
self_test
hits=$(mktemp "$ROOT_DIR/target/portable-path-hits.XXXXXX")
if ! scan_repository >"$hits"; then
    rm -f "$hits"
    exit 2
fi
if ! apply_allowlist "$ROOT_DIR/$ALLOWLIST_REL" <"$hits"; then
    rm -f "$hits"
    echo "portability check failed: replace literal homes/hosts with HOME, repo-relative paths, PATH lookup, or an explicit environment override" >&2
    # WARNING -- THE FIFTH REMEDY, AND IT EXISTS BECAUSE FOUR AGENTS GUESSED A
    # SIXTH. On 2026-08-25 five heads cleared this gate by removing a host
    # identity from a provenance record; four destroyed the information (#2646,
    # #2647, #2648, #2652) and one moved it correctly (#2526). Every remedy on
    # the line above substitutes something a PROGRAM resolves -- none fits a
    # hostname in a COMMENT recording where a measurement was taken, where the
    # literal is a fact rather than a path. Faced with four inapplicable options
    # and a red gate, the cheapest remaining action is deletion, and a
    # measurement whose host is unrecorded cannot be re-run, compared or
    # challenged. Naming the fifth option is what stops that.
    #
    # ⚠️ THE PARENTHETICAL BELOW SAYS "MARKDOWN", NOT "docs/", AND THAT IS A
    # CORRECTION. An earlier version of this message said "this checker does not
    # scan docs/", which is FALSE: `is_excluded` has no `docs/` entry, and the
    # scope predicate is `is_build_or_run_file "$path" || [[ -x ... ]]` -- by
    # FILE TYPE, not by directory. Measured on a tracked file: a host in
    # `docs/x.md` passes rc=0, the same host in an executable `docs/x.sh` FAILS
    # rc=1 and is named. Caught by a review lane. Sending someone to `docs/` on
    # the strength of the directory would red the gate on the next reader who
    # put provenance in a script there -- the same shape of wrong answer this
    # message exists to stop.
    echo "  ...or, when the literal is a MEASUREMENT PROVENANCE rather than a path: record the host in docs/TESTING_ENVIRONMENTS.md under \"Named measurement hosts\" (this checker scans files that BUILD or RUN, not Markdown prose) and reference that section from here." >&2
    echo "  DO NOT DELETE IT. A measurement with no host cannot be re-run, compared or challenged; that is what four fixes on 2026-08-25 cost." >&2
    echo "  ...or, ONLY for a detector's own needle (text a checker must contain so that it can refuse it, never a host that ran anything): add an entry, with its reason, to $ALLOWLIST_REL." >&2
    exit 1
fi
rm -f "$hits"

echo "Portability path check passed."
