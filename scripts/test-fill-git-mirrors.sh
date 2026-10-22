#!/usr/bin/env bash
# Regression test for scripts/fill-git-mirrors.sh.
#
# Fill: a mirror served through a url.<mirror>.insteadOf rule that lacks a
# locked commit gets it from the source URL without ".git", and a Cargo-style
# fetch through the rule then succeeds where it failed before. Each lock form
# Cargo writes (rev, branch, default branch) is parsed; the fill fetches no
# tags and writes no FETCH_HEAD. A mirror whose hooks would refuse every ref
# update is still filled, a file:// rule is followed, and a run with a git
# hook's GIT_DIR and related variables set fills the mirror and leaves the
# hook's repository alone. A commit present without its blob (a cut-off fetch)
# is fetched again. A non-bare mirror with a populated submodule and
# fetch.recurseSubmodules=true is filled without fetching the submodule. ssh
# runs with BatchMode=yes unless GIT_SSH_COMMAND, GIT_SSH or core.sshCommand
# names another command, which then runs unchanged. No fetch: without a rule,
# for a lock URL that is itself a local repository, with every locked commit
# present, for a rule that points at another remote, a relative path, or a
# directory inside a repository, for a URL starting with "-", and for a
# lockfile that is missing or has no git sources, the script prints nothing
# and changes no ref.
# Advisory: an unreachable upstream, a hung one (bounded by
# FILL_GIT_MIRRORS_TIMEOUT), and a URL with no form that escapes the rule (a
# URL without ".git", a rule on a directory prefix, or the mirror's own rule
# on the URL without ".git"), are each reported and exit 0. Only file://
# upstreams are exercised; no network fetch is tested.
# Wiring: scripts/check-default-build-warnings.sh fills the mirror before it
# runs cargo, and still runs cargo when the fill script is absent.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
fill=$script_dir/fill-git-mirrors.sh
[[ -x $fill ]] || { echo "test-fill-git-mirrors: missing executable $fill" >&2; exit 2; }

# Only the rules each case sets apply: no system or global configuration, and
# none inherited through the environment. Run from a git hook, the fixture
# commands below would otherwise act on the repository being pushed.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
unset GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS GIT_SSH GIT_SSH_COMMAND GIT_DIR GIT_WORK_TREE GIT_IMPLICIT_WORK_TREE \
    GIT_COMMON_DIR GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
    GIT_GRAFT_FILE GIT_NO_REPLACE_OBJECTS GIT_REPLACE_REF_BASE GIT_PREFIX GIT_SHALLOW_FILE GIT_NAMESPACE

tmp=$(mktemp -d)
trap 'rm -rf "${tmp:?}"' EXIT
failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }

fixture_git() {
    git -c user.email=fixture@example.invalid -c user.name=fixture \
        -c commit.gpgsign=false -c init.defaultBranch=main "$@"
}

up=$tmp/up/dep
fixture_git init -q "$up"
echo 1 >"$up/file"
fixture_git -C "$up" add file
fixture_git -C "$up" commit -qm one
rev1=$(git -C "$up" rev-parse HEAD)
echo 2 >"$up/file"
fixture_git -C "$up" commit -qam two
rev2=$(git -C "$up" rev-parse HEAD)
# A tag on the locked commit, which the fill must not bring along.
fixture_git -C "$up" tag -a -m v2 v2 "$rev2"

# A fresh mirror holding only rev1, as a mirror does once a pin moves past it.
new_mirror() {
    rm -rf "${tmp:?}/mirror.git"
    git init -q --bare "$tmp/mirror.git"
    git -C "$tmp/mirror.git" fetch -q --no-tags --no-write-fetch-head "$up" "+$rev1:refs/heads/main"
}
url=file://$up.git
rule=(GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.$tmp/mirror.git.insteadOf" "GIT_CONFIG_VALUE_0=$url")
lock() { # file, then one source line body each
    local file=$1; shift
    : >"$file"
    for source in "$@"; do printf '[[package]]\nname = "dep"\nsource = "git+%s"\n\n' "$source" >>"$file"; done
}
run() { # label, then env assignments and arguments for the script
    local label=$1; shift
    status=0
    env "$@" >"$tmp/$label.out" 2>"$tmp/$label.err" || status=$?
    [[ $status -eq 0 ]] || fail "$label: exit $status"
    [[ ! -s $tmp/$label.out ]] || fail "$label: wrote stdout: $(cat "$tmp/$label.out")"
}
filled_refs() { git -C "$tmp/mirror.git" for-each-ref --format='%(refname)' refs/fill-git-mirrors/ | wc -l; }
cargo_style_fetch() { # what Cargo with net.git-fetch-with-cli asks for, through the rule
    rm -rf "${tmp:?}/cargo-db"
    git init -q --bare "$tmp/cargo-db"
    env "${rule[@]}" git -C "$tmp/cargo-db" fetch -q --no-tags "$url" "+$rev2:refs/commit/$rev2" 2>/dev/null
}

# Fill, one lock line per form Cargo writes; the rule makes the fetch fail first.
for form in "?rev=$rev2#$rev2" "?branch=main#$rev2" "#$rev2"; do
    new_mirror
    if cargo_style_fetch; then fail "fill $form: the fixture mirror already serves $rev2"; fi
    lock "$tmp/Cargo.lock" "$url$form"
    run fill "${rule[@]}" "$fill" "$tmp/Cargo.lock"
    git -C "$tmp/mirror.git" cat-file -e "$rev2^{commit}" 2>/dev/null || fail "fill $form: mirror still lacks $rev2"
    [[ $(filled_refs) -eq 1 ]] || fail "fill $form: expected one refs/fill-git-mirrors ref, got $(filled_refs)"
    grep -q "fetched $rev2" "$tmp/fill.err" || fail "fill $form: no report: $(cat "$tmp/fill.err")"
    [[ -z $(git -C "$tmp/mirror.git" for-each-ref refs/tags/) ]] || fail "fill $form: fetched tags"
    [[ ! -e $tmp/mirror.git/FETCH_HEAD ]] || fail "fill $form: wrote FETCH_HEAD into the mirror"
    cargo_style_fetch || fail "fill $form: a Cargo-style fetch through the rule still fails"
done

# A rule written as a file:// URL names the same local mirror.
new_mirror
lock "$tmp/Cargo.lock" "$url?rev=$rev2#$rev2"
run file-rule GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.file://$tmp/mirror.git.insteadOf" \
    "GIT_CONFIG_VALUE_0=$url" "$fill" "$tmp/Cargo.lock"
[[ $(filled_refs) -eq 1 ]] || fail "file-rule: expected one refs/fill-git-mirrors ref, got $(filled_refs)"

# Run from a git hook, the environment points git at the repository being
# pushed. The mirror is still the one filled, and that repository is untouched.
new_mirror
git init -q --bare "$tmp/pushed.git"
run hook-env "${rule[@]}" "GIT_DIR=$tmp/pushed.git" "GIT_WORK_TREE=$tmp" \
    "GIT_OBJECT_DIRECTORY=$tmp/pushed.git/objects" "GIT_INDEX_FILE=$tmp/pushed.index" \
    "$fill" "$tmp/Cargo.lock"
[[ $(filled_refs) -eq 1 ]] || fail "hook-env: expected one refs/fill-git-mirrors ref, got $(filled_refs)"
git -C "$tmp/mirror.git" cat-file -e "$rev2^{commit}" 2>/dev/null || fail "hook-env: mirror still lacks $rev2"
[[ -z $(git -C "$tmp/pushed.git" for-each-ref) ]] || fail "hook-env: wrote refs into the pushed repository"
if git -C "$tmp/pushed.git" cat-file -e "$rev2^{commit}" 2>/dev/null; then fail "hook-env: fetched into the pushed repository"; fi

# Hooks that refuse every ref update do not stop the fill.
new_mirror
mkdir "$tmp/hooks"
printf '#!/bin/sh\nexit 1\n' >"$tmp/hooks/reference-transaction"
chmod +x "$tmp/hooks/reference-transaction"
git -C "$tmp/mirror.git" config core.hooksPath "$tmp/hooks"
lock "$tmp/Cargo.lock" "$url?rev=$rev2#$rev2"
run hooks "${rule[@]}" "$fill" "$tmp/Cargo.lock"
# The objects arrive before the ref update a hook can abort, so check the ref.
[[ $(filled_refs) -eq 1 ]] || fail "hooks: expected one refs/fill-git-mirrors ref, got $(filled_refs)"
grep -q "fetched $rev2" "$tmp/hooks.err" || fail "hooks: no report: $(cat "$tmp/hooks.err")"

# A commit whose tree arrived but whose blob did not, as a fetch cut off by
# the time limit leaves it, is missing and is fetched again.
new_mirror
git -C "$up" cat-file commit "$rev2" | git -C "$tmp/mirror.git" hash-object -t commit -w --stdin >/dev/null
git -C "$up" cat-file tree "$rev2^{tree}" | git -C "$tmp/mirror.git" hash-object -t tree -w --stdin >/dev/null
if cargo_style_fetch; then fail "partial: the fixture mirror already serves $rev2"; fi
lock "$tmp/Cargo.lock" "$url?rev=$rev2#$rev2"
run partial "${rule[@]}" "$fill" "$tmp/Cargo.lock"
[[ $(filled_refs) -eq 1 ]] || fail "partial: expected one refs/fill-git-mirrors ref, got $(filled_refs)"
grep -q "fetched $rev2" "$tmp/partial.err" || fail "partial: no report: $(cat "$tmp/partial.err")"
cargo_style_fetch || fail "partial: a Cargo-style fetch through the rule still fails"

# A non-bare mirror whose work tree has a populated submodule, and which asks
# for submodule recursion on every fetch, is filled without fetching the
# submodule. That submodule's upload-pack leaves a mark if it is ever run.
sm=$tmp/sm-mirror
fixture_git init -q "$sm"
fixture_git -C "$sm" -c protocol.file.allow=always submodule add -q "file://$up" inner
fixture_git -C "$sm" commit -qm inner
git -C "$sm" fetch -q --no-tags --no-write-fetch-head "$up" "+$rev1:refs/heads/dep"
git -C "$sm" config fetch.recurseSubmodules true
printf '#!/bin/sh\ntouch "%s"\nexec git-upload-pack "$@"\n' "$tmp/inner-fetched" >"$tmp/inner-upload-pack.sh"
chmod +x "$tmp/inner-upload-pack.sh"
git -C "$sm/inner" config remote.origin.uploadpack "$tmp/inner-upload-pack.sh"
run submodule GIT_CONFIG_COUNT=2 "GIT_CONFIG_KEY_0=url.$sm.insteadOf" "GIT_CONFIG_VALUE_0=$url" \
    GIT_CONFIG_KEY_1=protocol.file.allow GIT_CONFIG_VALUE_1=always "$fill" "$tmp/Cargo.lock"
[[ $(git -C "$sm" for-each-ref refs/fill-git-mirrors/ | wc -l) -eq 1 ]] || fail "submodule: the non-bare mirror was not filled: $(cat "$tmp/submodule.err")"
[[ ! -e $tmp/inner-fetched ]] || fail "submodule: fetched the mirror's submodule"

# ssh runs in batch mode, unless the user named an ssh command, which is used
# as given. The fake ssh commands record their arguments and refuse.
mkdir "$tmp/sshbin"
# shellcheck disable=SC2016 # $0 and $* belong to the fake ssh, not to this script
printf '#!/bin/sh\necho "${0##*/} $*" >>"%s"\nexit 1\n' "$tmp/ssh.log" >"$tmp/sshbin/ssh"
chmod +x "$tmp/sshbin/ssh"
cp "$tmp/sshbin/ssh" "$tmp/sshbin/my-ssh"
ssh_url=ssh://example.invalid/dep.git
lock "$tmp/ssh.lock" "$ssh_url?rev=$rev2#$rev2"
ssh_rule=(GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.$tmp/mirror.git.insteadOf" "GIT_CONFIG_VALUE_0=$ssh_url" "PATH=$tmp/sshbin:$PATH")
ssh_case() { # label, expected recorder, whether BatchMode is expected, then env assignments
    local label=$1 expect=$2 batch=$3; shift 3
    rm -f "$tmp/ssh.log"
    run "$label" "${ssh_rule[@]}" "$@" "$fill" "$tmp/ssh.lock"
    grep -q "could not fetch $rev2" "$tmp/$label.err" || fail "$label: no report: $(cat "$tmp/$label.err")"
    # Git may first run an ssh command it does not recognise with -G, to learn
    # its variant; every call must still go to the expected one.
    [[ $(cut -d' ' -f1 "$tmp/ssh.log" 2>/dev/null | sort -u) == "$expect" ]] ||
        fail "$label: expected only $expect to run, log: $(cat "$tmp/ssh.log" 2>/dev/null)"
    if grep -q BatchMode=yes "$tmp/ssh.log" 2>/dev/null; then
        [[ $batch == yes ]] || fail "$label: added BatchMode to the user's ssh command"
    else
        [[ $batch == no ]] || fail "$label: ssh ran without BatchMode"
    fi
}
new_mirror
ssh_case ssh-default ssh yes
ssh_case ssh-command my-ssh no "GIT_SSH_COMMAND=$tmp/sshbin/my-ssh"
ssh_case ssh-program my-ssh no "GIT_SSH=$tmp/sshbin/my-ssh"
git -C "$tmp/mirror.git" config core.sshCommand "$tmp/sshbin/my-ssh"
ssh_case ssh-config my-ssh no
[[ $(filled_refs) -eq 0 ]] || fail "ssh: changed the mirror"

# Advisory: the mirror's own rule on the URL without ".git" is a rewrite the
# fetch, which runs in the mirror, would follow, so the gap is reported.
new_mirror
git -C "$tmp/mirror.git" config "url.$tmp/elsewhere.insteadOf" "file://$up"
lock "$tmp/Cargo.lock" "$url?rev=$rev2#$rev2"
run mirror-rule "${rule[@]}" "$fill" "$tmp/Cargo.lock"
grep -q "no form of that URL escapes the rewrite" "$tmp/mirror-rule.err" || fail "mirror-rule: no report: $(cat "$tmp/mirror-rule.err")"
[[ $(filled_refs) -eq 0 ]] || fail "mirror-rule: changed the mirror"

# No fetch. Each case leaves the mirror as it was and prints nothing.
no_fetch() { # label, then env assignments and arguments
    local label=$1
    run "$@"
    [[ ! -s $tmp/$label.err ]] || fail "$label: wrote stderr: $(cat "$tmp/$label.err")"
    [[ $(filled_refs) -eq 0 ]] || fail "$label: changed the mirror"
    if git -C "$tmp/mirror.git" cat-file -e "$rev2^{commit}" 2>/dev/null; then fail "$label: fetched $rev2"; fi
}
new_mirror
lock "$tmp/Cargo.lock" "$url?rev=$rev2#$rev2"
no_fetch no-rule "$fill" "$tmp/Cargo.lock"
no_fetch other-remote GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.https://example.invalid/.insteadOf" \
    "GIT_CONFIG_VALUE_0=$url" "$fill" "$tmp/Cargo.lock"
# A lock URL that is itself a local repository, with no rule: not a mirror.
lock "$tmp/local.lock" "file://$tmp/mirror.git?rev=$rev2#$rev2"
no_fetch local-source "$fill" "$tmp/local.lock"
no_fetch missing-lockfile "${rule[@]}" "$fill" "$tmp/absent/Cargo.lock"
printf '[[package]]\nname = "x"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n' >"$tmp/registry.lock"
no_fetch no-git-sources "${rule[@]}" "$fill" "$tmp/registry.lock"
# Only an absolute path to a repository's own directory counts as a mirror: not
# a relative path (here one that would resolve to the mirror), and not a
# directory inside a repository.
no_fetch relative-rule -C "$tmp" GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.mirror.git.insteadOf" \
    "GIT_CONFIG_VALUE_0=$url" "$fill" "$tmp/Cargo.lock"
no_fetch inside-repo GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.$tmp/mirror.git/refs.insteadOf" \
    "GIT_CONFIG_VALUE_0=$url" "$fill" "$tmp/Cargo.lock"
# A lock URL that starts with "-" never reaches git as an option. Run from a
# clone of the mirror: there, `git ls-remote --get-url` taking the URL as an
# option would print the clone's origin, the mirror itself.
printf '#!/bin/sh\ntouch "%s"\n' "$tmp/dash-ran" >"$tmp/dash.sh"
chmod +x "$tmp/dash.sh"
git clone -q --bare "$tmp/mirror.git" "$tmp/clone.git"
lock "$tmp/dash.lock" "--upload-pack=$tmp/dash.sh?rev=$rev2#$rev2"
no_fetch leading-dash -C "$tmp/clone.git" GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.$tmp/mirror.git.insteadOf" \
    "GIT_CONFIG_VALUE_0=--upload-pack=$tmp/dash.sh" "$fill" "$tmp/dash.lock"
[[ ! -e $tmp/dash-ran ]] || fail "leading-dash: ran the lock URL as a git option"
# Every locked commit present: with the upstream gone, any fetch would be reported.
lock "$tmp/Cargo.lock" "$url?rev=$rev1#$rev1"
mv "$tmp/up" "$tmp/up-away"
no_fetch all-present "${rule[@]}" "$fill" "$tmp/Cargo.lock"

# Advisory: an unreachable upstream is reported, exit 0.
lock "$tmp/Cargo.lock" "$url?rev=$rev2#$rev2"
run unreachable "${rule[@]}" "$fill" "$tmp/Cargo.lock"
grep -q "could not fetch $rev2" "$tmp/unreachable.err" || fail "unreachable: no report: $(cat "$tmp/unreachable.err")"
mv "$tmp/up-away" "$tmp/up"

# Advisory: an upstream that never answers is abandoned at the time limit. Its
# HEAD is a FIFO with no writer, so opening it blocks every git that looks.
hung=$tmp/hung/dep
mkdir -p "$hung/objects" "$hung/refs"
mkfifo "$hung/HEAD"
lock "$tmp/hung.lock" "file://$hung.git?rev=$rev2#$rev2"
# The outer timeout only keeps a broken script from hanging this test.
start=$SECONDS
run hung GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.$tmp/mirror.git.insteadOf" \
    "GIT_CONFIG_VALUE_0=file://$hung.git" FILL_GIT_MIRRORS_TIMEOUT=2 timeout 20 "$fill" "$tmp/hung.lock"
((SECONDS - start < 15)) || fail "hung: took $((SECONDS - start)) s with a 2 s limit"
grep -q "could not fetch $rev2 .*: timed out after 2 s" "$tmp/hung.err" || fail "hung: no report: $(cat "$tmp/hung.err")"
if pgrep -f "$hung" >/dev/null; then fail "hung: left a process behind: $(pgrep -af "$hung")"; fi

# Advisory: a URL without ".git" has no form that escapes its own rule.
bare_url=file://$up
lock "$tmp/Cargo.lock" "$bare_url?rev=$rev2#$rev2"
run no-escape GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.$tmp/mirror.git.insteadOf" \
    "GIT_CONFIG_VALUE_0=$bare_url" "$fill" "$tmp/Cargo.lock"
grep -q "no form of that URL escapes the rewrite" "$tmp/no-escape.err" || fail "no-escape: no report: $(cat "$tmp/no-escape.err")"
[[ $(filled_refs) -eq 0 ]] || fail "no-escape: changed the mirror"

# Advisory: a rule on a directory prefix rewrites the URL without ".git" too.
mkdir "$tmp/mirrors"
mv "$tmp/mirror.git" "$tmp/mirrors/dep.git"
lock "$tmp/Cargo.lock" "$url?rev=$rev2#$rev2"
run prefix-rule GIT_CONFIG_COUNT=1 "GIT_CONFIG_KEY_0=url.$tmp/mirrors/.insteadOf" \
    "GIT_CONFIG_VALUE_0=file://$tmp/up/" "$fill" "$tmp/Cargo.lock"
grep -q "no form of that URL escapes the rewrite" "$tmp/prefix-rule.err" || fail "prefix-rule: no report: $(cat "$tmp/prefix-rule.err")"
if git -C "$tmp/mirrors/dep.git" cat-file -e "$rev2^{commit}" 2>/dev/null; then fail "prefix-rule: fetched $rev2"; fi
mv "$tmp/mirrors/dep.git" "$tmp/mirror.git"

# Wiring: the pre-push lint fills the mirror before it runs cargo, in both of
# its modes. A fake cargo records whether the mirror held the commit by then.
mkdir -p "$tmp/root/scripts" "$tmp/bin"
cp "$script_dir/check-default-build-warnings.sh" "$fill" "$tmp/root/scripts/"
lock "$tmp/root/Cargo.lock" "$url?rev=$rev2#$rev2"
printf '#!/bin/sh\nif git -C "%s" cat-file -e "%s^{commit}" 2>/dev/null; then echo filled; else echo unfilled; fi >"%s"\n' \
    "$tmp/mirror.git" "$rev2" "$tmp/cargo-saw" >"$tmp/bin/cargo"
chmod +x "$tmp/bin/cargo"
for mode in exec --quiet; do
    new_mirror
    rm -f "$tmp/cargo-saw"
    arg=()
    [[ $mode == exec ]] || arg=("$mode")
    run "lint-$mode" "${rule[@]}" "PATH=$tmp/bin:$PATH" "$tmp/root/scripts/check-default-build-warnings.sh" "${arg[@]}"
    [[ $(cat "$tmp/cargo-saw" 2>/dev/null) == filled ]] ||
        fail "lint $mode: cargo ran before the mirror held $rev2 ($(cat "$tmp/cargo-saw" 2>/dev/null || echo 'cargo never ran'))"
done
# A missing fill script does not fail the lint: cargo still runs and decides.
rm "$tmp/root/scripts/fill-git-mirrors.sh"
new_mirror
rm -f "$tmp/cargo-saw"
run lint-no-fill "${rule[@]}" "PATH=$tmp/bin:$PATH" "$tmp/root/scripts/check-default-build-warnings.sh" --quiet
[[ $(cat "$tmp/cargo-saw" 2>/dev/null) == unfilled ]] ||
    fail "lint without the fill script: cargo did not run on the unfilled mirror"

if [[ $failures -ne 0 ]]; then
    echo "test-fill-git-mirrors: $failures failure(s)" >&2
    exit 1
fi
echo "test-fill-git-mirrors: ok"
