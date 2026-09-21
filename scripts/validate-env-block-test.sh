#!/usr/bin/env bash
# Compatibility entrypoint for the environmental-failure classifier bracket.
# The classifier and its two-sided fixtures now live together in the Rust
# validation driver, so this script cannot drift by copying or scraping them.

set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output=$("$root/scripts/validate.rs" --self-test)
grep -Fq 'runtime: environmental classifier bracketed' <<<"$output"
printf '%s\n' "$output"
echo "validate-env-block-test: Rust classifier bracket passed"
