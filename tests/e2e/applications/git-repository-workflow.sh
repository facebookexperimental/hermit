#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

case ${1:-} in
    --prepare)
        test -x /usr/bin/git || {
            echo "/usr/bin/git not found" >&2
            exit 1
        }
        for tool in sha256sum tar; do
            command -v "$tool" >/dev/null 2>&1 || {
                echo "$tool not found" >&2
                exit 1
            }
        done
        ;;
    --run)
        work="${E2E_TMPDIR:-/tmp}/hermit-git-repository-workflow"
        repo="$work/repo"
        git_bin=/usr/bin/git
        rm -rf -- "$work"
        mkdir -p -- "$repo" "$work/home" "$work/xdg"

        # Keep the transaction independent of developer and system Git config.
        export HOME="$work/home"
        export XDG_CONFIG_HOME="$work/xdg"
        export GIT_CONFIG_NOSYSTEM=1
        export GIT_AUTHOR_NAME='Hermit Corpus'
        export GIT_AUTHOR_EMAIL='hermit-corpus@example.invalid'
        export GIT_COMMITTER_NAME=$GIT_AUTHOR_NAME
        export GIT_COMMITTER_EMAIL=$GIT_AUTHOR_EMAIL

        "$git_bin" init -q --initial-branch=main "$repo"
        "$git_bin" -C "$repo" config commit.gpgsign false
        "$git_bin" -C "$repo" config core.autocrlf false

        printf 'alpha\nbeta\ngamma\n' >"$repo/records.txt"
        printf '{"schema":1,"enabled":true}\n' >"$repo/config.json"
        "$git_bin" -C "$repo" add records.txt config.json
        GIT_AUTHOR_DATE='2000-01-01T00:00:00Z' \
            GIT_COMMITTER_DATE='2000-01-01T00:00:00Z' \
            "$git_bin" -C "$repo" commit -q -m 'seed deterministic corpus'

        printf 'delta\n' >>"$repo/records.txt"
        mkdir -p -- "$repo/nested"
        printf 'payload-v2\n' >"$repo/nested/payload.txt"
        "$git_bin" -C "$repo" add records.txt nested/payload.txt
        GIT_AUTHOR_DATE='2000-01-02T00:00:00Z' \
            GIT_COMMITTER_DATE='2000-01-02T00:00:00Z' \
            "$git_bin" -C "$repo" commit -q -m 'extend deterministic corpus'

        "$git_bin" -C "$repo" diff --quiet
        "$git_bin" -C "$repo" diff --cached --quiet
        "$git_bin" -C "$repo" fsck --no-dangling --no-progress >/dev/null
        "$git_bin" -C "$repo" archive --format=tar --output="$work/head.tar" HEAD

        printf 'GIT commits=%s status=clean\n' "$("$git_bin" -C "$repo" rev-list --count HEAD)"
        printf 'GIT files=%s\n' "$("$git_bin" -C "$repo" ls-tree -r --name-only HEAD | paste -sd, -)"
        printf 'GIT payload=%s\n' "$("$git_bin" -C "$repo" show HEAD:nested/payload.txt)"
        printf 'GIT archive_entries=%s\n' "$(tar -tf "$work/head.tar" | paste -sd, -)"
        printf 'GIT archive_sha256=%s\n' "$(sha256sum "$work/head.tar" | cut -d' ' -f1)"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
