#!/usr/bin/env bash
# Refuse references to the private parent workspace in tracked public files.
#
# Hermit is public. Several files once pointed readers at things only the
# maintainers' private workspace holds: issues and documents in the private
# rrnewton/dev-hermit repository, the workspace's directory layout
# (dev-hermit/..., worktrees/slots/..., an agent's temp/<agent>-status.md), and
# prose such as "from the checkout at dev-hermit/experiments/gvisor" that
# records where an agent happened to work rather than anything a reader can
# use. A public reader can open none of them, so a reference must instead say,
# in the file itself, what the private source said, or link a public Hermit
# issue.
#
# Rules, each applied to every line of every tracked text file outside the
# vendored and generated trees:
#
#   phrasing  "from the checkout at", or "from|in|at our|my checkout|slot|
#             worktree|lane": where the author worked, not a fact about Hermit.
#   link      a github.com/rrnewton/dev-hermit or
#             raw.githubusercontent.com/rrnewton/dev-hermit URL (issue, blob,
#             tree, commit, raw file), in any letter case: the repository is
#             private.
#   path      a path into the private workspace: dev-hermit/..., ~/work/
#             dev-hermit, worktrees/slots/..., or temp/<agent>-status.md.
#
# Not refused, by design:
#   - the bare repository slug "rrnewton/dev-hermit", as used by workflows
#     and adapters that read the private repository with a token (for example
#     "repos/rrnewton/dev-hermit/contents/..." in merge-gate.yml). That is a
#     functional API path, not a link a reader is asked to follow;
#   - ci/vendor/dev-hermit/, a directory of this repository;
#   - the placeholder "<dev-hermit>/", which some evidence notes use to say
#     "the private workspace" without naming any host or home directory;
#   - Hermit's own ignored/ output directory (ignored/validate/..., ignored/e2e/
#     ...), which every checkout has.
#
# A line that must keep a match goes in scripts/private-workspace-refs-allowlist.txt
# with a reason. An allowlist entry that no longer matches anything fails the
# check, so the list cannot silently outlive its reasons.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT_DIR
readonly ALLOWLIST_REL=scripts/private-workspace-refs-allowlist.txt

# Read "path:line:content" records on stdin; print "path:line: [rule] content"
# for every record that breaks a rule. Exit 0 always.
classify() {
    awk '
        {
            path = $0
            sub(/:.*/, "", path)
            rest = substr($0, length(path) + 2)
            line = rest
            sub(/:.*/, "", line)
            content = substr(rest, length(line) + 2)

            lower = tolower(content)
            rule = ""
            if (lower ~ /from (the|our|my) checkout at/ ||
                lower ~ /(from|in|at) (our|my) (checkout|slot|worktree|lane)([^[:alnum:]_-]|$)/) {
                rule = "phrasing"
            } else if (lower ~ /github(usercontent)?\.com\/rrnewton\/dev-hermit([^[:alnum:]_.-]|$)/) {
                rule = "link"
            } else {
                # GitHub owner and repository names are case-insensitive, so
                # every comparison below is made on the lowercased line.
                probe = lower
                gsub(/rrnewton\/dev-hermit/, "", probe)
                gsub(/vendor\/dev-hermit\//, "vendor/", probe)
                if (probe ~ /(^|[^[:alnum:]_.<-])dev-hermit\// ||
                    probe ~ /work\/dev-hermit([^[:alnum:]_.-]|$)/ ||
                    probe ~ /(^|[^[:alnum:]_.-])worktrees\/slots\// ||
                    probe ~ /(^|[^[:alnum:]_.-])temp\/[[:alnum:]_.<>-]+-status\.md/) {
                    rule = "path"
                }
            }
            if (rule != "") {
                print path ":" line ": [" rule "] " content
            }
        }
    '
}

# Read classify output on stdin and drop the hits the allowlist file $1 covers.
# Print the remaining hits, then any malformed or stale allowlist entry as
# "ALLOWLIST:<n>: ...". Exit 1 when anything was printed.
apply_allowlist() {
    local allowlist=$1
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
            content = $0
            sub(/^[^ ]* \[[a-z]+\] /, "", content)
            allowed = 0
            for (i = 1; i <= n; i++) {
                prefix = entry_path[i]
                matched = (path == prefix) ||
                    (substr(prefix, length(prefix)) == "/" && index(path, prefix) == 1)
                if (matched && index(content, entry_text[i]) > 0) {
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

scan_repository() {
    git -C "$ROOT_DIR" grep -n -I -i -E \
        -e 'checkout at' \
        -e '(our|my) (checkout|slot|worktree|lane)' \
        -e 'dev-hermit' \
        -e 'worktrees/slots/' \
        -e 'temp/[^ ]*-status\.md' \
        -- . \
        ':(exclude,glob)**/vendor/**' \
        ':(exclude,glob)**/third-party/**' \
        ':(exclude,glob)**/target/**' \
        ":(exclude)$ALLOWLIST_REL" \
        ':(exclude)scripts/check-private-workspace-refs.sh' \
        | classify
}

expect_hit() {
    local rule=$1 text=$2 out
    out=$(printf 'fixture.rs:1:%s\n' "$text" | classify)
    if [[ $out != "fixture.rs:1: [$rule] "* ]]; then
        echo "private-workspace self-test: expected a [$rule] hit for: $text" >&2
        echo "  got: ${out:-<nothing>}" >&2
        return 1
    fi
}

expect_clean() {
    local text=$1 out
    out=$(printf 'fixture.rs:1:%s\n' "$text" | classify)
    if [[ -n $out ]]; then
        echo "private-workspace self-test: expected no hit for: $text" >&2
        echo "  got: $out" >&2
        return 1
    fi
}

# The fixtures are literal text; nothing in them is meant to expand.
# shellcheck disable=SC2016
self_test() {
    expect_hit phrasing 'cloned from the checkout at dev-hermit/experiments/gvisor'
    expect_hit phrasing 'Measured From the checkout at /src'
    expect_hit phrasing 'built in our checkout at HEAD'
    expect_hit phrasing 'the logs are in my slot.'
    expect_hit link '// https://github.com/rrnewton/dev-hermit/issues/549'
    expect_hit link '[doc](https://github.com/rrnewton/dev-hermit/blob/main/ai_docs/x.md)'
    expect_hit link 'see github.com/rrnewton/dev-hermit for details'
    expect_hit link 'https://github.com/rrnewton/Dev-Hermit/issues/549'
    expect_hit link 'https://GitHub.com/RRNewton/DEV-HERMIT/blob/main/x.md'
    expect_hit link 'https://raw.githubusercontent.com/rrnewton/dev-hermit/main/ci-hub/x.py'
    expect_hit path 'cd ~/work/Dev-Hermit/hermit'
    expect_hit path 'cd ~/work/dev-hermit/hermit'
    expect_hit path 'exec $HOME/work/dev-hermit'
    expect_hit path 'provide dev-hermit/bin/safehermit explicitly'
    expect_hit path 'kept under worktrees/slots/kvm/ignored/x'
    expect_hit path 'contract recorded in temp/liteinst-status.md'
    expect_hit path 'write temp/<agent>-status.md'

    expect_clean 'repos/rrnewton/dev-hermit/contents/ci-hub/check_outcome.py?ref=abc'
    expect_clean 'repository: rrnewton/dev-hermit'
    expect_clean 'repos/RRNewton/Dev-Hermit/contents/ci-hub/check_outcome.py'
    expect_clean 'https://github.com/rrnewton/dev-hermit-demo-suite'
    expect_clean '"parent_repository": "rrnewton/dev-hermit",'
    expect_clean 'ci/vendor/dev-hermit/ci-hub/check_outcome.py'
    expect_clean 'dev-hermit-demo-suite/ignored/demo08-btrfs'
    expect_clean '`<dev-hermit>/ignored/hermit-132-results/`'
    expect_clean 'retains ignored/validate/artifacts/<run-id>/coverage.json'
    expect_clean 'read in the checkout at `root`, and whether'
    expect_clean 'https://github.com/rrnewton/hermit/issues/3301'
    expect_clean 'absent from the dev-hermit repository'"'"'s main'
    expect_clean 'a temp/ directory and a status.md file'
    expect_clean 'the leader slot'"'"'s former occupant'

    # The allowlist: an entry hides only lines of its own path that contain its
    # text, a malformed or stale entry fails, and a directory entry ends in /.
    local fixture out rc
    fixture=$(mktemp)
    printf '%s\n' '# comment' \
        'a.yml | repos/x | functional' \
        '.claude/skills/ | ~/work/dev-hermit | owner-only files' >"$fixture"
    out=$(printf '%s\n' \
        'a.yml:3: [path] use repos/x/dev-hermit/y' \
        '.claude/skills/s/SKILL.md:9: [path] at ~/work/dev-hermit/ai_docs' \
        'b.yml:4: [path] use repos/x/dev-hermit/y' | apply_allowlist "$fixture") && rc=0 || rc=$?
    if [[ $rc -ne 1 || $out != 'b.yml:4: [path] use repos/x/dev-hermit/y' ]]; then
        echo "private-workspace self-test: allowlist filtering is wrong (rc=$rc): $out" >&2
        rm -f "$fixture"
        return 1
    fi
    out=$(printf '%s\n' 'a.yml:3: [path] use repos/x/dev-hermit/y' | apply_allowlist "$fixture") && rc=0 || rc=$?
    if [[ $rc -ne 1 || $out != *'stale entry'*'.claude/skills/'* ]]; then
        echo "private-workspace self-test: a stale allowlist entry was not reported (rc=$rc): $out" >&2
        rm -f "$fixture"
        return 1
    fi
    printf '%s\n' 'a.yml | repos/x |' >"$fixture"
    out=$(printf '%s\n' 'a.yml:3: [path] use repos/x/dev-hermit/y' | apply_allowlist "$fixture") && rc=0 || rc=$?
    if [[ $rc -ne 1 || $out != *'malformed entry'* ]]; then
        echo "private-workspace self-test: an entry without a reason was accepted (rc=$rc): $out" >&2
        rm -f "$fixture"
        return 1
    fi
    rm -f "$fixture"
}

self_test
if [[ ${1:-} == --self-test ]]; then
    echo "Private workspace reference self-test passed."
    exit 0
fi
if ! scan_repository | apply_allowlist "$ROOT_DIR/$ALLOWLIST_REL"; then
    cat >&2 <<'EOF'
private workspace reference check failed. Hermit is public, and the lines above
point readers at the maintainers' private workspace, which they cannot open.
  - A link into rrnewton/dev-hermit: replace it with a sentence saying what the
    linked issue or document established, or link a public Hermit issue that
    covers the same thing.
  - A workspace path or "from the checkout at ...": say what was measured or
    where it lives in this repository, not where an agent worked. Evidence that
    must stay host-local can say "<dev-hermit>/..." for the private workspace.
  - A functional use that must keep the text: add an entry, with its reason, to
    scripts/private-workspace-refs-allowlist.txt.
EOF
    exit 1
fi
echo "Private workspace reference check passed."
