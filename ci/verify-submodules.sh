#!/usr/bin/env bash
# Verify every configured submodule without initializing or repairing it.

set -euo pipefail

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

valid_submodule_path() {
    local path=$1 component
    local -a components
    [[ -n $path && $path != /* && $path != */ && $path != *//* \
        && $path != *$'\n'* && $path != *$'\r'* ]] || return 1
    IFS='/' read -r -a components <<<"$path"
    for component in "${components[@]}"; do
        [[ -n $component && $component != . && $component != .. ]] || return 1
    done
}

check_submodules() (
    local prefix=${1:-} recursive=${2:-0}
    local git_command=${SUBMODULE_GIT:-git}
    local -a git_argv
    read -r -a git_argv <<<"$git_command"
    ((${#git_argv[@]} > 0)) || fail 'SUBMODULE_GIT is empty'

    local expected_file tree_file index_file status_file
    expected_file=$(mktemp)
    tree_file=$(mktemp)
    index_file=$(mktemp)
    status_file=$(mktemp)
    trap 'rm -f -- "$expected_file" "$tree_file" "$index_file" "$status_file"' EXIT

    if ! GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
        ls-tree -rz --full-tree HEAD >"$tree_file"; then
        fail "could not read committed gitlinks from HEAD: ${prefix:-.}"
    fi

    local -a configured_records=() tree_records=()
    mapfile -d '' -t tree_records <"$tree_file"
    local -A configured=() gitlinks=()
    local -a configured_paths=()
    local record key path metadata mode type sha extra
    for record in "${tree_records[@]}"; do
        [[ $record == *$'\t'* ]] || fail 'malformed git tree entry'
        metadata=${record%%$'\t'*}
        path=${record#*$'\t'}
        read -r mode type sha extra <<<"$metadata"
        [[ -z ${extra:-} ]] || fail "malformed git tree metadata for $path"
        [[ $mode == 160000 ]] || continue
        [[ $type == commit && $sha =~ ^[0-9a-f]{40}$ ]] || \
            fail "malformed committed gitlink for $path"
        valid_submodule_path "$path" || fail "unsafe or malformed committed gitlink path: $path"
        [[ -z ${gitlinks[$path]+present} ]] || fail "duplicate committed gitlink path: $path"
        gitlinks[$path]=$sha
    done

    # Every level uses its own committed tree, index and declared paths.
    # A leaf may omit .gitmodules only when its tree contains no gitlinks.
    if [[ ! -e .gitmodules && ! -L .gitmodules ]]; then
        [[ $recursive == 1 && ${#gitlinks[@]} == 0 ]] || \
            fail ".gitmodules is missing or unreadable: ${prefix:-.}"
        return 0
    fi
    [[ -f .gitmodules && -r .gitmodules && ! -L .gitmodules ]] || \
        fail ".gitmodules is not a readable regular file: ${prefix:-.}"
    local config_status=0
    GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
        config -z -f .gitmodules --get-regexp '^submodule\..*\.path$' >"$expected_file" || config_status=$?
    if ((config_status != 0)); then
        [[ $config_status == 1 && $recursive == 1 && ${#gitlinks[@]} == 0 ]] || \
            fail "could not read the configured submodule paths from .gitmodules: ${prefix:-.}"
        return 0
    fi
    mapfile -d '' -t configured_records <"$expected_file"
    ((${#configured_records[@]} > 0)) || fail ".gitmodules declares no submodules: ${prefix:-.}"

    for record in "${configured_records[@]}"; do
        [[ $record == *$'\n'* ]] || fail 'malformed submodule path entry in .gitmodules'
        key=${record%%$'\n'*}
        path=${record#*$'\n'}
        [[ $key == submodule.*.path ]] || fail "malformed submodule path key: $key"
        valid_submodule_path "$path" || fail "unsafe or malformed submodule path: $path"
        [[ -z ${configured[$path]+present} ]] || fail "duplicate configured submodule path: $path"
        configured[$path]=1
        configured_paths+=("$path")
    done

    ((${#configured[@]} == ${#gitlinks[@]})) || \
        fail ".gitmodules declares ${#configured[@]} path(s), but HEAD contains ${#gitlinks[@]} gitlink(s)"
    for path in "${!gitlinks[@]}"; do
        [[ -n ${configured[$path]+present} ]] || \
            fail "committed gitlink is not declared in .gitmodules: $path"
    done

    local observed=0 nested_root expected_root nested_head dirty display_path component part
    local -a index_records=() path_parts=()
    for path in "${configured_paths[@]}"; do
        display_path=$prefix$path
        [[ -n ${gitlinks[$path]+present} ]] || \
            fail "configured path is not a committed gitlink in HEAD: $display_path"
        sha=${gitlinks[$path]}

        : >"$index_file"
        if ! GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false --literal-pathspecs \
            ls-files --stage -z -- "$path" >"$index_file"; then
            fail "could not inspect the index entry for submodule: $display_path"
        fi
        index_records=()
        mapfile -d '' -t index_records <"$index_file"
        ((${#index_records[@]} == 1)) || \
            fail "submodule index entry is missing or conflicted for $display_path (${#index_records[@]} stage rows)"
        record=${index_records[0]}
        [[ $record == *$'\t'* ]] || fail "malformed submodule index entry for $display_path"
        metadata=${record%%$'\t'*}
        [[ ${record#*$'\t'} == "$path" ]] || fail "submodule index path mismatch for $display_path"
        read -r mode nested_head type extra <<<"$metadata"
        [[ -z ${extra:-} && $mode == 160000 && $type == 0 && $nested_head == "$sha" ]] || \
            fail "submodule index entry does not match HEAD gitlink for $display_path"

        component=
        IFS='/' read -r -a path_parts <<<"$path"
        for part in "${path_parts[@]}"; do
            component=${component:+$component/}$part
            [[ ! -L $component ]] || fail "submodule path contains a symlink: $display_path"
        done
        [[ -d $path && ! -L $path ]] || fail "submodule directory is missing or not a directory: $display_path"
        if ! nested_root=$(GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
            -C "$path" rev-parse --show-toplevel); then
            fail "submodule path is not a populated git repository: $display_path"
        fi
        expected_root=$(cd -- "$path" && pwd -P)
        nested_root=$(cd -- "$nested_root" && pwd -P)
        [[ $nested_root == "$expected_root" ]] || \
            fail "submodule path resolves through a different git repository: $display_path"
        if ! nested_head=$(GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
            -C "$path" rev-parse --verify 'HEAD^{commit}'); then
            fail "submodule has no readable HEAD commit: $display_path"
        fi
        [[ $nested_head == "$sha" ]] || \
            fail "submodule HEAD differs from recorded gitlink for $display_path: expected $sha, found $nested_head"

        # Recurse without consulting mutable submodule registration or repairing
        # anything. This repeats the same path/tree/index/root/HEAD/clean checks
        # for descendants, including unregistered but populated repositories.
        # Check descendants before aggregate dirt so a missing or altered leaf
        # is reported at its actual path; the parent clean check still follows.
        (cd -- "$path" && check_submodules "$display_path/" 1)

        : >"$status_file"
        if ! GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
            -C "$path" status --porcelain=v1 --untracked-files=all --ignore-submodules=none \
            >"$status_file"; then
            fail "could not inspect submodule worktree state: $display_path"
        fi
        dirty=$(<"$status_file")
        [[ -z $dirty ]] || fail "submodule worktree is dirty: $display_path"

        printf ' %s %s\n' "$sha" "$display_path"
        ((observed += 1))
    done

    if [[ -z $prefix ]]; then
        printf 'submodules OK -- %d configured path(s), verified against HEAD and populated worktrees without repair\n' \
            "$observed"
    fi
)

self_test() {
    local root self git_bin agent_seed rr_seed nested_seed leaf_seed super agent_sha rr_sha output
    # Git exports its repository-location variables to hooks and `git rebase
    # --exec` steps, and they override `git -C`. Every fixture below is named
    # explicitly, so run without them; otherwise a scratch `git init` rewrites
    # the caller's repository (https://github.com/rrnewton/hermit/issues/3362).
    # The verifier runs under test are children, so they inherit this too.
    unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY \
        GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_PREFIX
    root=$(mktemp -d)
    self=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")
    git_bin=$(command -v git)
    agent_seed=$root/agent-utils-seed
    rr_seed=$root/rr-seed
    super=$root/super
    output=$root/output
    trap 'rm -rf -- "$root"' RETURN

    make_seed() {
        local repo=$1 payload=$2
        "$git_bin" init -q "$repo"
        printf '%s\n' "$payload" >"$repo/payload"
        "$git_bin" -C "$repo" add payload
        "$git_bin" -C "$repo" -c user.name=fixture -c user.email=fixture@example.invalid \
            commit -qm initial
    }
    make_seed "$agent_seed" agent-utils
    make_seed "$rr_seed" rr
    agent_sha=$("$git_bin" -C "$agent_seed" rev-parse HEAD)
    rr_sha=$("$git_bin" -C "$rr_seed" rev-parse HEAD)

    "$git_bin" init -q "$super"
    mkdir -p "$super/third-party"
    cat >"$super/.gitmodules" <<EOF
[submodule "agent-utils"]
    path = agent-utils
    url = $agent_seed
[submodule "rr"]
    path = third-party/rr
    url = $rr_seed
EOF
    "$git_bin" -C "$super" add .gitmodules
    "$git_bin" -C "$super" update-index --add --cacheinfo "160000,$agent_sha,agent-utils"
    "$git_bin" -C "$super" update-index --add --cacheinfo "160000,$rr_sha,third-party/rr"
    "$git_bin" -C "$super" -c user.name=fixture -c user.email=fixture@example.invalid \
        commit -qm super

    make_checkout() {
        local checkout=$root/$1
        "$git_bin" clone -q --no-recurse-submodules "$super" "$checkout"
        "$git_bin" clone -q --no-recurse-submodules "$agent_seed" "$checkout/agent-utils"
        mkdir -p "$checkout/third-party"
        "$git_bin" clone -q "$rr_seed" "$checkout/third-party/rr"
        printf '%s\n' "$checkout"
    }
    run_check() {
        local checkout=$1
        (cd "$checkout" && SUBMODULE_GIT="$git_bin" "$self" --check) >"$output" 2>&1
    }
    expect_refusal() {
        local label=$1 checkout=$2
        if run_check "$checkout"; then
            cat "$output" >&2
            fail "self-test $label unexpectedly passed"
        fi
    }

    local checkout config_before config_after
    checkout=$(make_checkout clean)
    if "$git_bin" -C "$checkout" config --local --get-regexp '^submodule\..*\.' >/dev/null; then
        fail 'self-test clean fixture unexpectedly registered submodules'
    fi
    config_before=$(cksum <"$checkout/.git/config")
    run_check "$checkout" || { cat "$output" >&2; fail 'self-test clean unregistered checkout failed'; }
    config_after=$(cksum <"$checkout/.git/config")
    [[ $config_before == "$config_after" ]] || fail 'self-test verifier mutated local git config'
    if "$git_bin" -C "$checkout" config --local --get-regexp '^submodule\..*\.' >/dev/null; then
        fail 'self-test verifier registered submodules while checking them'
    fi
    grep -q 'verified against HEAD and populated worktrees without repair' "$output" || \
        fail 'self-test clean success marker absent'

    checkout=$(make_checkout missing)
    rm -rf -- "$checkout/third-party/rr"
    expect_refusal 'missing directory' "$checkout"

    checkout=$(make_checkout nonrepo)
    rm -rf -- "$checkout/agent-utils"
    mkdir "$checkout/agent-utils"
    expect_refusal 'non-repository directory' "$checkout"

    checkout=$(make_checkout wrong-head)
    printf '%s\n' changed >>"$checkout/agent-utils/payload"
    "$git_bin" -C "$checkout/agent-utils" add payload
    "$git_bin" -C "$checkout/agent-utils" -c user.name=fixture -c user.email=fixture@example.invalid \
        commit -qm changed
    expect_refusal 'wrong clean HEAD' "$checkout"

    checkout=$(make_checkout dirty)
    printf '%s\n' dirty >>"$checkout/agent-utils/payload"
    expect_refusal 'dirty worktree' "$checkout"

    checkout=$(make_checkout conflict)
    "$git_bin" -C "$checkout" update-index --force-remove agent-utils
    printf '160000 %s 1\tagent-utils\0' "$agent_sha" | \
        "$git_bin" -C "$checkout" update-index -z --index-info
    printf '160000 %s 2\tagent-utils\0' "$rr_sha" | \
        "$git_bin" -C "$checkout" update-index -z --index-info
    printf '160000 %s 3\tagent-utils\0' "$agent_sha" | \
        "$git_bin" -C "$checkout" update-index -z --index-info
    expect_refusal 'conflicted gitlink' "$checkout"

    checkout=$(make_checkout malformed)
    cat >"$checkout/.gitmodules" <<EOF
[submodule "escape"]
    path = ../escape
    url = $agent_seed
EOF
    expect_refusal 'unsafe configured path' "$checkout"

    checkout=$(make_checkout producer-failure)
    if (cd "$checkout" && SUBMODULE_GIT=false "$self" --check) >"$output" 2>&1; then
        fail 'self-test git producer failure unexpectedly passed'
    fi

    # Registration-free status can miss an absent descendant. Keep that
    # opponent, then prove fully populated descendants pass the same checks.
    nested_seed=$root/nested-seed
    leaf_seed=$root/leaf-seed
    make_seed "$leaf_seed" leaf
    make_seed "$nested_seed" nested
    "$git_bin" -c protocol.file.allow=always -C "$nested_seed" submodule add -q \
        "$leaf_seed" deps/deep
    "$git_bin" -C "$nested_seed" -c user.name=fixture -c user.email=fixture@example.invalid \
        commit -qam deep
    "$git_bin" -c protocol.file.allow=always -C "$agent_seed" submodule add -q \
        "$nested_seed" nested
    "$git_bin" -C "$agent_seed" -c user.name=fixture -c user.email=fixture@example.invalid \
        commit -qam nested
    agent_sha=$("$git_bin" -C "$agent_seed" rev-parse HEAD)
    "$git_bin" -C "$super" update-index --cacheinfo "160000,$agent_sha,agent-utils"
    "$git_bin" -C "$super" -c user.name=fixture -c user.email=fixture@example.invalid \
        commit -qm nested
    checkout=$(make_checkout nested)
    if "$git_bin" -C "$checkout/agent-utils" config --local \
        --get-regexp '^submodule\..*\.' >/dev/null; then
        fail 'self-test nested fixture unexpectedly registered its descendant'
    fi
    config_before=$(cksum <"$checkout/agent-utils/.git/config")
    expect_refusal 'missing committed nested submodule' "$checkout"
    config_after=$(cksum <"$checkout/agent-utils/.git/config")
    [[ $config_before == "$config_after" ]] || \
        fail 'self-test nested refusal mutated local git config'
    grep -q 'agent-utils/nested' "$output" || \
        fail 'self-test nested refusal did not name the committed nested path'

    make_nested_checkout() {
        local checkout
        checkout=$(make_checkout "$1")
        "$git_bin" clone -q --no-recurse-submodules "$nested_seed" "$checkout/agent-utils/nested"
        mkdir -p "$checkout/agent-utils/nested/deps"
        "$git_bin" clone -q "$leaf_seed" "$checkout/agent-utils/nested/deps/deep"
        printf '%s\n' "$checkout"
    }
    nested_configs() {
        local checkout=$1 repo
        for repo in "$checkout" "$checkout/agent-utils" "$checkout/agent-utils/nested" \
            "$checkout/agent-utils/nested/deps/deep"; do
            if "$git_bin" -C "$repo" config --local --get-regexp '^submodule\..*\.' >/dev/null; then
                fail "self-test recursive fixture registered a submodule: $repo"
            fi
            cksum <"$repo/.git/config"
        done
    }
    # Commit a deliberately malformed descendant contract through the fixture
    # ancestors, so its refusal cannot be explained by an unrelated dirty tree.
    commit_nested_contract() {
        local checkout=$1 repo
        for repo in "$checkout/agent-utils/nested" "$checkout/agent-utils" "$checkout"; do
            "$git_bin" -C "$repo" add -A
            "$git_bin" -C "$repo" -c user.name=fixture -c user.email=fixture@example.invalid \
                commit -qm 'fixture descendant contract'
        done
    }

    checkout=$(make_nested_checkout recursive-clean)
    config_before=$(nested_configs "$checkout")
    run_check "$checkout" || { cat "$output" >&2; fail 'self-test populated recursive checkout failed'; }
    config_after=$(nested_configs "$checkout")
    [[ $config_before == "$config_after" ]] || fail 'self-test recursive success mutated local Git configuration'
    grep -q 'agent-utils/nested/deps/deep' "$output" || fail 'self-test omitted the deepest verified path'

    checkout=$(make_nested_checkout recursive-missing)
    rm -rf -- "$checkout/agent-utils/nested/deps/deep"
    expect_refusal 'missing deepest checkout' "$checkout"
    grep -q 'agent-utils/nested/deps/deep' "$output" || fail 'self-test deepest refusal omitted its path'

    checkout=$(make_nested_checkout recursive-foreign-root)
    rm -rf -- "$checkout/agent-utils/nested/deps/deep/.git"
    expect_refusal 'deepest checkout resolves through its parent repository' "$checkout"
    grep -q 'resolves through a different git repository' "$output" || fail 'self-test missed foreign repository identity'

    checkout=$(make_nested_checkout recursive-wrong-head)
    printf '%s\n' changed >>"$checkout/agent-utils/nested/deps/deep/payload"
    "$git_bin" -C "$checkout/agent-utils/nested/deps/deep" \
        -c user.name=fixture -c user.email=fixture@example.invalid commit -qam changed
    expect_refusal 'wrong deepest HEAD' "$checkout"
    grep -q 'HEAD differs from recorded gitlink' "$output" || fail 'self-test missed the descendant pin mismatch'

    local dirty_kind
    for dirty_kind in unstaged staged untracked; do
        checkout=$(make_nested_checkout "recursive-$dirty_kind")
        if [[ $dirty_kind == untracked ]]; then
            printf '%s\n' dirty >"$checkout/agent-utils/nested/deps/deep/untracked"
        else
            printf '%s\n' dirty >>"$checkout/agent-utils/nested/deps/deep/payload"
            if [[ $dirty_kind == staged ]]; then
                "$git_bin" -C "$checkout/agent-utils/nested/deps/deep" add payload
            fi
        fi
        expect_refusal "deepest $dirty_kind worktree" "$checkout"
        grep -q 'worktree is dirty: agent-utils/nested/deps/deep' "$output" || \
            fail "self-test missed deepest $dirty_kind dirt"
    done

    checkout=$(make_nested_checkout recursive-conflict)
    "$git_bin" -C "$checkout/agent-utils/nested" update-index --force-remove deps/deep
    printf '160000 %s 1\tdeps/deep\0' "$rr_sha" | \
        "$git_bin" -C "$checkout/agent-utils/nested" update-index -z --index-info
    expect_refusal 'conflicted descendant index' "$checkout"
    grep -q 'index entry does not match HEAD gitlink' "$output" || fail 'self-test missed the descendant index conflict'

    checkout=$(make_nested_checkout recursive-symlink)
    rm -rf -- "$checkout/agent-utils/nested/deps/deep"
    ln -s "$leaf_seed" "$checkout/agent-utils/nested/deps/deep"
    expect_refusal 'symlinked descendant repository' "$checkout"
    grep -q 'path contains a symlink' "$output" || fail 'self-test missed the descendant symlink'

    checkout=$(make_nested_checkout recursive-symlink-component)
    mv "$checkout/agent-utils/nested/deps" "$root/moved-deps"
    ln -s "$root/moved-deps" "$checkout/agent-utils/nested/deps"
    expect_refusal 'symlinked descendant path component' "$checkout"
    grep -q 'path contains a symlink' "$output" || fail 'self-test missed the intermediate symlink'

    checkout=$(make_nested_checkout recursive-undeclared)
    rm "$checkout/agent-utils/nested/.gitmodules"
    commit_nested_contract "$checkout"
    expect_refusal 'committed descendant gitlink without declarations' "$checkout"
    grep -q '.gitmodules is missing or unreadable' "$output" || fail 'self-test missed the undeclared descendant gitlink'

    checkout=$(make_nested_checkout recursive-unsafe)
    "$git_bin" -C "$checkout/agent-utils/nested" config -f .gitmodules submodule.deps/deep.path ../escape
    commit_nested_contract "$checkout"
    expect_refusal 'unsafe committed descendant path' "$checkout"
    grep -q 'unsafe or malformed submodule path' "$output" || fail 'self-test missed unsafe descendant path'

    printf 'PASS: verify-submodules checks unregistered populated recursive repositories and refuses missing, foreign, wrong-HEAD, dirty, conflicted, symlinked, undeclared, malformed, and unreadable states without repair\n'
}

case "${1:---check}" in
    --check) check_submodules ;;
    --self-test) self_test ;;
    *) fail "usage: $0 [--check|--self-test]" ;;
esac
