#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)

[[ ${1-} == --log && -n ${2-} && ${3-} == run && ${4-} == -- ]] || {
  printf 'unexpected runner invocation\n' >&2
  exit 2
}

log_level=$2
shift 4
exec timeout 30s "$repo_root/target/debug/hermit" --log "$log_level" run \
  --no-sequentialize-threads \
  --no-deterministic-io \
  --preemption-timeout=disabled \
  -- "$@"
