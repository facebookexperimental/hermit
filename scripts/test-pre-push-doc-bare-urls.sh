#!/usr/bin/env bash
# The pre-push hook's bare-URL step, with no cargo or rust-script run: both
# checkers are stubbed so only the hook's own handling of the bare-URL
# checker's exit status is under test. The checker itself is tested by its own
# unit tests (check.script_unit_tests) and by `make lint-checks`.
#
# Why the hook runs it at all: validation node doc.rustdoc denies rustdoc's
# bare_urls lint, but soft-green landing validates only after the push, so a
# bare URL reached `main` repeatedly (6166181d8f is one). The hook runs before
# the push, but only in a checkout that ran scripts/setup-hooks.sh; a merge
# made on GitHub runs no hook.
#
# ⚠️ FOUR OUTCOMES, NOT TWO. Findings must refuse and name the remedy; a checker
# that could not run must refuse WITHOUT claiming findings, including when it
# failed to compile, which rust-script reports as exit 1 just like findings; a
# missing checker is a stated skip, never a silent pass; and a branch deletion
# must not run it.
set -uo pipefail
# Git exports its repository-location variables to hooks and `git rebase
# --exec` steps, and they override `git -C`. Every repository below is named
# explicitly, so run without them; otherwise a scratch `git init` rewrites the
# caller's repository (https://github.com/rrnewton/hermit/issues/3362).
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY \
    GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_PREFIX

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
HOOK=$ROOT/.githooks/pre-push
[[ -f $HOOK ]] || { echo "FAIL: hook not found at $HOOK" >&2; exit 1; }

# The stub below prints the real checker's verdict line. If the checker's
# wording changed, the hook would report findings as COULD NOT RUN, so bind
# the two strings here.
grep -q '"check-doc-bare-urls: FAILED -- ' "$ROOT/scripts/check-doc-bare-urls.rs" ||
    { echo "FAIL: scripts/check-doc-bare-urls.rs no longer prints the verdict line the hook matches" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

repo="$tmp/repo"
mkdir -p "$repo/scripts"
git -C "$repo" init -q
git -C "$repo" config user.email t@example.invalid
git -C "$repo" config user.name test

# A build checker that passes and records that it ran.
cat > "$repo/scripts/check-default-build-warnings.sh" <<'STUB'
#!/usr/bin/env bash
touch "$(dirname "$0")/../build-check-ran"
exit 0
STUB
chmod +x "$repo/scripts/check-default-build-warnings.sh"

# A bare-URL checker whose exit status the case chooses.
cat > "$repo/scripts/check-doc-bare-urls.rs" <<'STUB'
#!/usr/bin/env bash
touch "$(dirname "$0")/../doc-check-ran"
printf '%s' "${XDG_CACHE_HOME:-}" > "$(dirname "$0")/../doc-check-cache"
status=$(cat "$(dirname "$0")/../doc-check-status")
case $status in
    1) echo 'src/bin/x.rs:1: bare URL https://example.com/a in a doc comment' >&2
       echo 'check-doc-bare-urls: FAILED -- 1 bare URL(s) in 1 documented source files.' >&2 ;;
    # rust-script's own exit status when the checker does not compile.
    compile) echo 'error[E0308]: mismatched types' >&2; exit 1 ;;
    2) echo 'check-doc-bare-urls: ERROR -- cargo metadata failed' >&2 ;;
    127) echo 'env: rust-script: No such file or directory' >&2 ;;
esac
exit "$status"
STUB
chmod +x "$repo/scripts/check-doc-bare-urls.rs"
printf 'seed\n' > "$repo/seed.txt"
git -C "$repo" add -A
git -C "$repo" commit -qm seed
head=$(git -C "$repo" rev-parse HEAD)
zero=0000000000000000000000000000000000000000

fail() { echo "FAIL: $1" >&2; exit 1; }

# run_hook STATUS STDIN_LINE: sets $out and $hook_status.
run_hook() {
    printf '%s\n' "$1" > "$repo/doc-check-status"
    rm -f "$repo/doc-check-ran" "$repo/build-check-ran"
    out=$( (cd "$repo" && printf '%s\n' "$2" | bash "$HOOK" origin https://example.invalid) 2>&1)
    hook_status=$?
}
push="refs/heads/x $head refs/heads/x $zero"

# ---- findings refuse, name the remedy, and stop before the build check.
run_hook 1 "$push"
[[ $hook_status -ne 0 ]] || fail "a bare URL must refuse the push"
[[ $out == *"bare URL https://example.com/a"* ]] || fail "the checker's own finding must reach the user; got: $out"
[[ $out == *"<https://...>"* ]] || fail "the refusal must name the fix; got: $out"
[[ $out != *"COULD NOT RUN"* ]] || fail "findings were relabelled as could-not-run"
[[ ! -e $repo/build-check-ran ]] || fail "the hook must stop at the first refusal"

# ---- a checker that could not run refuses without claiming findings.
run_hook 2 "$push"
[[ $hook_status -ne 0 ]] || fail "a checker error must refuse the push, not pass it"
[[ $out == *"COULD NOT RUN (exit 2)"* ]] || fail "a checker error must say so; got: $out"
[[ $out == *"cargo metadata failed"* ]] || fail "the checker's own error must reach the user; got: $out"
[[ $out != *"has a bare URL"* ]] || fail "a checker error was reported as a finding"

# ---- a missing rust-script names the remedy.
run_hook 127 "$push"
[[ $hook_status -ne 0 ]] || fail "a missing rust-script must refuse the push, not pass it"
[[ $out == *"COULD NOT RUN (exit 127)"* ]] || fail "a missing rust-script must say the check could not run; got: $out"
[[ $out == *"cargo install rust-script"* ]] || fail "a missing rust-script must name the remedy; got: $out"

# ---- exit 1 without the checker's verdict line is a checker that did not
# compile, which rust-script also reports as 1; it is not a finding.
run_hook compile "$push"
[[ $hook_status -ne 0 ]] || fail "a checker that did not compile must refuse the push, not pass it"
[[ $out == *"COULD NOT RUN (exit 1)"* ]] || fail "a compile failure must say the check could not run; got: $out"
[[ $out != *"has a bare URL"* ]] || fail "a compile failure was reported as a finding"

# ---- a clean tree passes through to the build check.
run_hook 0 "$push"
[[ $hook_status -eq 0 ]] || fail "a clean tree with a passing build must push; got: $out"
[[ -e $repo/doc-check-ran ]] || fail "the bare-URL checker did not run"
# The checker gets a rust-script cache of its own: in the shared one it waited
# out every other script's compile, past the 300 s timeout under load.
[[ $(cat "$repo/doc-check-cache") == "${XDG_CACHE_HOME:-$HOME/.cache}/hermit-pre-push" ]] ||
    fail "the bare-URL checker must run with its own rust-script cache; got: $(cat "$repo/doc-check-cache")"
[[ -e $repo/build-check-ran ]] || fail "the build check did not run after a clean bare-URL check"

# ---- a branch deletion pushes no code and runs neither checker.
run_hook 1 "refs/heads/x $zero refs/heads/x $head"
[[ $hook_status -eq 0 ]] || fail "a deletion must not be blocked; got: $out"
[[ ! -e $repo/doc-check-ran ]] || fail "a deletion ran the bare-URL checker"

# ---- a missing checker is a stated skip, not a silent pass.
rm "$repo/scripts/check-doc-bare-urls.rs"
run_hook 1 "$push"
[[ $hook_status -eq 0 ]] || fail "a missing checker must not block a push whose build passes; got: $out"
[[ $out == *"This is a skip, NOT a pass."* ]] || fail "a missing checker must be reported as a skip; got: $out"

echo "PASS: pre-push refuses a bare URL, refuses a checker that could not run without claiming findings, and states a missing checker as a skip"
