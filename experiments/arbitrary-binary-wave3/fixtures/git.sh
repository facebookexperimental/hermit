#!/bin/sh

set -eu

root=/tmp/hermit-wave3-git
mkdir "$root"
cd "$root"

printf 'phase=git-init\n'
git init -q
git config user.name Wave3
git config user.email wave3@example.invalid

printf 'tracked\n' >tracked.txt
git add tracked.txt
printf 'phase=git-commit\n'
GIT_AUTHOR_DATE='2001-01-01T00:00:00Z' \
GIT_COMMITTER_DATE='2001-01-01T00:00:00Z' \
  git commit -q -m baseline

printf 'changed\n' >>tracked.txt
printf 'new\n' >untracked.txt
printf 'phase=git-status\n'
git status --porcelain=v1 --untracked-files=all
