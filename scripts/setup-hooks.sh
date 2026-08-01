#!/usr/bin/env bash
# Install Hermit's tracked pre-commit checks for this clone/worktree repository.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit

echo "core.hooksPath -> .githooks"
echo "Active: Reverie pin freshness pre-commit gate"
echo "Policy: docs/updating-reverie.md"
