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

check_submodules() {
    local git_command=${SUBMODULE_GIT:-git}
    local -a git_argv
    read -r -a git_argv <<<"$git_command"
    ((${#git_argv[@]} > 0)) || fail 'SUBMODULE_GIT is empty'

    [[ -r .gitmodules ]] || fail '.gitmodules is missing or unreadable'

    local expected_file tree_file index_file status_file
    expected_file=$(mktemp)
    tree_file=$(mktemp)
    index_file=$(mktemp)
    status_file=$(mktemp)
    trap 'rm -f -- "$expected_file" "$tree_file" "$index_file" "$status_file"' RETURN

    if ! GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
        config -z -f .gitmodules --get-regexp '^submodule\..*\.path$' >"$expected_file"; then
        fail 'could not read the configured submodule paths from .gitmodules'
    fi
    if ! GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
        ls-tree -rz --full-tree HEAD >"$tree_file"; then
        fail 'could not read committed gitlinks from HEAD'
    fi

    local -a configured_records=() tree_records=()
    mapfile -d '' -t configured_records <"$expected_file"
    mapfile -d '' -t tree_records <"$tree_file"
    ((${#configured_records[@]} > 0)) || fail '.gitmodules declares no submodules'

    local -A configured=() gitlinks=()
    local -a configured_paths=()
    local record key path metadata mode type sha extra
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

    ((${#configured[@]} == ${#gitlinks[@]})) || \
        fail ".gitmodules declares ${#configured[@]} path(s), but HEAD contains ${#gitlinks[@]} gitlink(s)"
    for path in "${!gitlinks[@]}"; do
        [[ -n ${configured[$path]+present} ]] || \
            fail "committed gitlink is not declared in .gitmodules: $path"
    done

    local observed=0 nested_root expected_root nested_head dirty
    local nested_record nested_metadata nested_mode nested_type nested_sha nested_extra nested_path
    local -a index_records=() nested_tree_records=()
    for path in "${configured_paths[@]}"; do
        [[ -n ${gitlinks[$path]+present} ]] || \
            fail "configured path is not a committed gitlink in HEAD: $path"
        sha=${gitlinks[$path]}

        : >"$index_file"
        if ! GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false --literal-pathspecs \
            ls-files --stage -z -- "$path" >"$index_file"; then
            fail "could not inspect the index entry for submodule: $path"
        fi
        index_records=()
        mapfile -d '' -t index_records <"$index_file"
        ((${#index_records[@]} == 1)) || \
            fail "submodule index entry is missing or conflicted for $path (${#index_records[@]} stage rows)"
        record=${index_records[0]}
        [[ $record == *$'\t'* ]] || fail "malformed submodule index entry for $path"
        metadata=${record%%$'\t'*}
        [[ ${record#*$'\t'} == "$path" ]] || fail "submodule index path mismatch for $path"
        read -r mode nested_head type extra <<<"$metadata"
        [[ -z ${extra:-} && $mode == 160000 && $type == 0 && $nested_head == "$sha" ]] || \
            fail "submodule index entry does not match HEAD gitlink for $path"

        [[ -d $path && ! -L $path ]] || fail "submodule directory is missing or not a directory: $path"
        if ! nested_root=$(GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
            -C "$path" rev-parse --show-toplevel); then
            fail "submodule path is not a populated git repository: $path"
        fi
        expected_root=$(cd -- "$path" && pwd -P)
        nested_root=$(cd -- "$nested_root" && pwd -P)
        [[ $nested_root == "$expected_root" ]] || \
            fail "submodule path resolves through a different git repository: $path"
        if ! nested_head=$(GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
            -C "$path" rev-parse --verify 'HEAD^{commit}'); then
            fail "submodule has no readable HEAD commit: $path"
        fi
        [[ $nested_head == "$sha" ]] || \
            fail "submodule HEAD differs from recorded gitlink for $path: expected $sha, found $nested_head"

        : >"$status_file"
        if ! GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
            -C "$path" status --porcelain=v1 --untracked-files=all --ignore-submodules=none \
            >"$status_file"; then
            fail "could not inspect submodule worktree state: $path"
        fi
        dirty=$(<"$status_file")
        [[ -z $dirty ]] || fail "submodule worktree is dirty: $path"

        # The old `git submodule status --recursive` gate refused an absent or
        # drifted descendant. Registration-free recursion needs to interpret
        # each nested repository's own .gitmodules and index; until that exists,
        # refuse any committed nested gitlink rather than silently dropping the
        # recursive part of the contract.
        : >"$tree_file"
        if ! GIT_OPTIONAL_LOCKS=0 "${git_argv[@]}" -c core.fsmonitor=false \
            -C "$path" ls-tree -rz --full-tree HEAD >"$tree_file"; then
            fail "could not inspect committed nested gitlinks under submodule: $path"
        fi
        nested_tree_records=()
        mapfile -d '' -t nested_tree_records <"$tree_file"
        for nested_record in "${nested_tree_records[@]}"; do
            [[ $nested_record == *$'\t'* ]] || \
                fail "malformed nested git tree entry under submodule: $path"
            nested_metadata=${nested_record%%$'\t'*}
            nested_path=${nested_record#*$'\t'}
            read -r nested_mode nested_type nested_sha nested_extra <<<"$nested_metadata"
            [[ $nested_mode == 160000 ]] || continue
            [[ -z ${nested_extra:-} && $nested_type == commit \
                && $nested_sha =~ ^[0-9a-f]{40}$ ]] || \
                fail "malformed committed nested gitlink: $path/$nested_path"
            valid_submodule_path "$nested_path" || \
                fail "unsafe or malformed committed nested gitlink path: $path/$nested_path"
            fail "committed nested submodule is not yet supported by the registration-free verifier: $path/$nested_path"
        done

        printf ' %s %s\n' "$sha" "$path"
        ((observed += 1))
    done

    printf 'submodules OK -- %d configured path(s), verified against HEAD and populated worktrees without repair\n' \
        "$observed"
}

self_test() {
    local root self git_bin agent_seed rr_seed nested_seed super agent_sha rr_sha output
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

    # Preserve the recursive refusal from the prior `git submodule status
    # --recursive` implementation. A populated top-level repository does not
    # report its absent nested checkout as dirty, so the committed gitlink must
    # be inspected directly rather than inferred from `git status`.
    nested_seed=$root/nested-seed
    make_seed "$nested_seed" nested
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
    expect_refusal 'committed nested submodule' "$checkout"
    config_after=$(cksum <"$checkout/agent-utils/.git/config")
    [[ $config_before == "$config_after" ]] || \
        fail 'self-test nested refusal mutated local git config'
    grep -q 'agent-utils/nested' "$output" || \
        fail 'self-test nested refusal did not name the committed nested path'

    printf 'PASS: verify-submodules checks unregistered populated repositories and refuses missing, non-repo, wrong-HEAD, dirty, conflicted, nested, malformed, and unreadable states without repair\n'
}

case "${1:---check}" in
    --check) check_submodules ;;
    --self-test) self_test ;;
    *) fail "usage: $0 [--check|--self-test]" ;;
esac
