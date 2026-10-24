#!/usr/bin/env bash
# Fill local git mirrors with the commits Cargo.lock pins, before Cargo asks
# them for those commits.
#
# Some hosts serve git dependencies from a local mirror through a
# url.<mirror>.insteadOf rule, for example a build environment whose proxy
# refuses the git host. Cargo with net.git-fetch-with-cli runs the git
# executable, which applies the rule, so Cargo asks the mirror for each locked
# commit. A mirror that predates a pin answers "upload-pack: not our ref", and
# the build fails although the commit is upstream.
#
# For each git source in each LOCKFILE, this script asks git for the
# effective URL (`git ls-remote --get-url`). When a rule turns it into the
# absolute path of a local repository, it fetches every locked commit that the
# mirror lacks. The fetch goes to the source URL with its trailing ".git"
# removed, provided git leaves that form unrewritten; otherwise the gap is
# reported and not filled. Each fetched commit is kept under
# refs/fill-git-mirrors/ so a later gc keeps it.
#
# What it guarantees, stated plainly so it is not overread:
# - It does nothing on a host without such a rule.
# - It makes no fetch when every locked commit is present.
# - It is advisory. A gap it cannot fill is reported on stderr and the exit
#   status stays 0, so Cargo still reports its own error. Each fetch is bounded
#   by FILL_GIT_MIRRORS_TIMEOUT seconds (default 60). Git never prompts for
#   credentials, and ssh runs in batch mode unless GIT_SSH_COMMAND, GIT_SSH or
#   core.sshCommand names another ssh command, which is then used unchanged.
# - It never changes what is built: Cargo.lock pins each git source to a
#   commit, and this script only decides where those objects come from.
#
# Usage: scripts/fill-git-mirrors.sh LOCKFILE...
set -uo pipefail

# A git hook runs with GIT_DIR (and from a linked worktree, more) pointing at
# the repository being pushed, which would override every `git -C "$mirror"`
# below. Configuration variables (git -c) stay: Cargo's git sees them too.
unset GIT_DIR GIT_WORK_TREE GIT_IMPLICIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE \
    GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_GRAFT_FILE \
    GIT_NO_REPLACE_OBJECTS GIT_REPLACE_REF_BASE GIT_PREFIX GIT_SHALLOW_FILE GIT_NAMESPACE
export GIT_TERMINAL_PROMPT=0
limit=${FILL_GIT_MIRRORS_TIMEOUT:-60}

err=$(mktemp)
trap 'rm -f "$err"' EXIT

for lockfile in "$@"; do
    [[ -f $lockfile ]] && sed -n 's/^source = "git+\([^?#"[:space:]]*\)[^#"[:space:]]*#\([0-9a-f]\{40\}\)"$/\1 \2/p' "$lockfile"
done | LC_ALL=C sort -u | while read -r url rev; do
    # A URL starting with "-" would reach git as an option.
    [[ $url != -* ]] || continue
    mirror=$(git ls-remote --get-url "$url" 2>/dev/null) || continue
    [[ $mirror != "$url" ]] || continue
    mirror=${mirror#file://}
    # Only an absolute path that is itself a repository, as upload-pack would
    # take it; git -C alone would also accept a directory inside one.
    [[ $mirror == /* ]] || continue
    gitdir=$(git -C "$mirror" rev-parse --absolute-git-dir 2>/dev/null) || continue
    gitdir=$(realpath -e -- "$gitdir" 2>/dev/null) || continue
    real=$(realpath -e -- "$mirror" 2>/dev/null) || continue
    [[ $gitdir == "$real" || $gitdir == "$real/.git" ]] || continue
    # Present means the commit and every object it needs. A fetch cut off by
    # the time limit can leave the commit without its trees or blobs, and a
    # commit-only check would then skip it for good.
    git -C "$mirror" rev-list --quiet --objects "$rev" --not --all 2>/dev/null && continue
    upstream=${url%.git}
    if [[ $(git -C "$mirror" ls-remote --get-url "$upstream" 2>/dev/null) != "$upstream" ]]; then
        echo "fill-git-mirrors: $mirror lacks $rev for $url, and no form of that URL escapes the rewrite; not filled" >&2
        continue
    fi
    # No hooks, no submodule recursion (defensive: the mirror may be another
    # checkout's submodule directory), no FETCH_HEAD and no gc in a mirror
    # that other checkouts use, and no tags: this fetch is about one commit.
    # ssh gets BatchMode only when nobody chose an ssh command. Git takes
    # GIT_SSH_COMMAND over core.sshCommand over GIT_SSH, so the setting below
    # could only override the latter two.
    ssh=()
    [[ -n ${GIT_SSH-}$(git -C "$mirror" config core.sshCommand 2>/dev/null) ]] ||
        ssh=(-c "core.sshCommand=ssh -o BatchMode=yes")
    timeout "$limit" git -C "$mirror" -c core.hooksPath=/dev/null -c gc.auto=0 -c maintenance.auto=false "${ssh[@]}" \
        fetch -q --no-tags --no-recurse-submodules --no-write-fetch-head \
        "$upstream" "+$rev:refs/fill-git-mirrors/$rev" 2>"$err" </dev/null
    status=$?
    if [[ $status -eq 0 ]]; then
        echo "fill-git-mirrors: fetched $rev into $mirror from $upstream" >&2
    elif [[ $status -eq 124 ]]; then
        echo "fill-git-mirrors: could not fetch $rev from $upstream into $mirror: timed out after ${limit} s" >&2
    else
        echo "fill-git-mirrors: could not fetch $rev from $upstream into $mirror: $(tail -n 1 "$err")" >&2
    fi
done
exit 0
