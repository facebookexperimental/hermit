#!/usr/bin/env bash
# Install Hermit's tracked pre-commit and pre-push checks for this
# clone/worktree repository.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit
chmod +x .githooks/pre-push
chmod +x scripts/check-default-build-warnings.sh
chmod +x scripts/check-doc-bare-urls.rs

echo "core.hooksPath -> .githooks"
echo "Active (pre-commit): Reverie pin consistency gate and forward-advance advisory"
echo "Active (pre-push):   bare URLs in the doc comments rustdoc lints"
echo "  scripts/check-doc-bare-urls.rs; doc.rustdoc denies the lint, but soft-green"
echo "  landing validates only after the push. Only pushes from a checkout that ran"
echo "  this script are checked; a merge made on GitHub is not. Needs rust-script."
echo "Active (pre-push):   default-feature build with warnings denied"
echo "  Runs the same command as CI's lint.clippy node, which local testing does"
echo "  not cover: cargo test does not set -D warnings and every workflow does."
echo "  Measured 2026-08-24 on a 316-core x86_64 Linux build host, green tree:"
echo "  0.3s warm, 1.0s after touching one source file, 25s from a cold"
echo "  CARGO_TARGET_DIR. Bypass: git push --no-verify"
echo "Policy: docs/updating-reverie.md"
