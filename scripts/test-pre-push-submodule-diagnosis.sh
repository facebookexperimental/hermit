#!/usr/bin/env bash
# Both directions of the pre-push hook's "could not check" vs "does not compile"
# distinction, with no cargo run: the checker is stubbed so only the hook's own
# branch selection is under test.
#
# The defect was measured 2026-09-04 on the host recorded for this check in
# docs/TESTING_ENVIRONMENTS.md under "Named measurement hosts". In a fresh
# detached worktree the submodules are unpopulated, Cargo cannot resolve a path
# dependency whose directory is absent, and the hook announced "the working tree
# does not compile in the default feature configuration" pointing at
# `cargo clippy`. It fired on a diff touching no Rust at all and never named the
# submodule. A clean detached worktree is the landing procedure CLAUDE.md
# prescribes, so the documented safe path reliably produced a misleading failure.
#
# ⚠️ BOTH DIRECTIONS. A hook that always blamed the submodule would pass the
# first case and hide every real compile failure, which is worse than the bug.
set -uo pipefail

HOOK=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)/.githooks/pre-push
[[ -f $HOOK ]] || { echo "FAIL: hook not found at $HOOK" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

repo="$tmp/repo"
mkdir -p "$repo/scripts"
git -C "$repo" init -q
git -C "$repo" config user.email t@example.invalid
git -C "$repo" config user.name test

# A checker that always fails, so the hook always reaches its diagnosis branch.
# Which message it then chooses is the whole subject of this test.
cat > "$repo/scripts/check-default-build-warnings.sh" <<'STUB'
#!/usr/bin/env bash
echo 'error: function `unused_for_test` is never used' >&2
exit 1
STUB
chmod +x "$repo/scripts/check-default-build-warnings.sh"
printf 'seed\n' > "$repo/seed.txt"
git -C "$repo" add -A
git -C "$repo" commit -qm seed
head=$(git -C "$repo" rev-parse HEAD)
stdin_line="refs/heads/x $head refs/heads/x 0000000000000000000000000000000000000000"

run_hook() {
    ( cd "$repo" && printf '%s\n' "$stdin_line" | bash "$HOOK" origin https://example.invalid ) 2>&1
}

fail() { echo "FAIL: $1" >&2; exit 1; }

# ---- direction 2 first: no submodules at all, so a stub failure is a genuine
# ---- compile failure and must be reported as one.
out=$(run_hook)
hook_status=$?
[[ $hook_status -ne 0 ]] || fail "a genuine checker failure must refuse the push"
[[ $out == *"does not compile in the default feature"* ]] ||
    fail "a genuine checker failure must still report as a compile failure; got: $out"
[[ $out != *"COULD NOT BE CHECKED"* ]] ||
    fail "a genuine compile failure was relabelled as could-not-check"

# ---- mixed direction: an OPTIONAL uninitialised submodule must not hide a
# ---- genuine checker failure. third-party/rr is not needed by the default
# ---- workspace clippy command.
cat > "$repo/.gitmodules" <<'MODULES'
[submodule "third-party/rr"]
	path = third-party/rr
	url = https://example.invalid/rr.git
MODULES
git -C "$repo" add .gitmodules
git -C "$repo" update-index --add --cacheinfo "160000,$head,third-party/rr"
git -C "$repo" commit -qm "record an optional uninitialised submodule"

out=$(run_hook)
hook_status=$?
[[ $hook_status -ne 0 ]] || fail "a genuine failure with optional rr absent must refuse"
[[ $out == *"does not compile in the default feature"* ]] ||
    fail "optional rr absence hid a genuine compile failure; got: $out"
[[ $out != *"COULD NOT BE CHECKED"* ]] ||
    fail "optional rr absence was incorrectly treated as the checker failure"

# ---- direction 1: a REQUIRED uninitialised submodule recorded in the index.
# ---- This is exactly what `git submodule status` prefixes with '-' in a fresh
# ---- worktree. Keep optional rr absent too so this case proves the diagnosis
# ---- names only the required submodule.
cat >> "$repo/.gitmodules" <<'MODULES'
[submodule "agent-utils"]
	path = agent-utils
	url = https://example.invalid/agent-utils.git
MODULES
git -C "$repo" add .gitmodules
git -C "$repo" update-index --add --cacheinfo "160000,$head,agent-utils"
git -C "$repo" commit -qm "record a required uninitialised submodule"

[[ $(git -C "$repo" submodule status | grep -c '^-') -eq 2 ]] ||
    fail "fixture did not produce both uninitialised submodules"

out=$(run_hook)
hook_status=$?
[[ $hook_status -ne 0 ]] || fail "could-not-check must still refuse the push"
[[ $out == *"COULD NOT BE CHECKED"* ]] ||
    fail "an unpopulated submodule must not be reported as a compile failure; got: $out"
[[ $out == *"agent-utils"* ]] ||
    fail "the message must name the submodule"
[[ $out != *"third-party/rr"* ]] ||
    fail "the message must not blame an unrelated optional submodule"
[[ $out == *"git submodule update --init agent-utils"* ]] ||
    fail "the message must give the exact remedy"
[[ $out != *"does not compile in the default feature"* ]] ||
    fail "the misleading compile-failure message must be suppressed"

# ---- diagnosis failure: an unreadable submodule state is neither proof that
# ---- compilation failed nor permission to push.
printf '[broken\n' > "$repo/.gitmodules"
out=$(run_hook)
hook_status=$?
[[ $hook_status -ne 0 ]] || fail "failed submodule diagnosis must refuse the push"
[[ $out == *"SUBMODULE DIAGNOSIS FAILED"* ]] ||
    fail "failed git submodule status must be reported explicitly; got: $out"
[[ $out != *"does not compile in the default feature"* ]] ||
    fail "failed diagnosis was incorrectly relabelled as a compile failure"

echo "PASS: pre-push names an unpopulated submodule, and still reports a real compile failure as one"
