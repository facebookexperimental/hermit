#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C

repository=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
known_failure_manifest="$repository/hermit-cli/tests/fail_closed_known_failures.tsv"
allowed_ignore_manifest="$repository/hermit-cli/tests/fail_closed_allowed_ignores.tsv"
cargo_args=("$@")
cargo_bin=${CARGO:-cargo}

fail() {
  printf 'fail-closed ratchet: %s\n' "$*" >&2
  exit 1
}

run_cargo() {
  local stderr_file status
  stderr_file=$(mktemp "${TMPDIR:-/tmp}/hermit-fail-closed-cargo.XXXXXX")
  if "$cargo_bin" "$@" 2>"$stderr_file"; then
    rm -f "$stderr_file"
  else
    status=$?
    cat "$stderr_file" >&2
    rm -f "$stderr_file"
    return "$status"
  fi
}

cd "$repository"
[[ -f "$known_failure_manifest" ]] || fail "missing $known_failure_manifest"
[[ -f "$allowed_ignore_manifest" ]] || fail "missing $allowed_ignore_manifest"

mapfile -t targets < <(
  find hermit-cli/tests -maxdepth 1 -type f -name '*.rs' -printf '%f\n' \
    | sed 's/\.rs$//' \
    | sort
)
((${#targets[@]} > 0)) || fail "no Hermit integration-test targets found"

declare -A target_exists=()
for target in "${targets[@]}"; do
  target_exists["$target"]=1
done

declare -A known_failures=()
previous_key=
while IFS=$'\t' read -r target test failure_class reason; do
  [[ -z "$target" || "$target" == \#* ]] && continue
  [[ -n "$test" && -n "$failure_class" && -n "$reason" ]] \
    || fail "malformed row in $known_failure_manifest"
  [[ -n "${target_exists[$target]+set}" ]] \
    || fail "unknown target '$target' in $known_failure_manifest"
  key="$target/$test"
  [[ -z "${known_failures[$key]+set}" ]] \
    || fail "duplicate known-failure row for $key"
  [[ -z "$previous_key" || "$previous_key" < "$key" ]] \
    || fail "$known_failure_manifest must remain sorted"
  known_failures["$key"]="$failure_class: $reason"
  previous_key="$key"
done < "$known_failure_manifest"

declare -A allowed_ignores=()
previous_key=
while IFS=$'\t' read -r target test reason; do
  [[ -z "$target" || "$target" == \#* ]] && continue
  [[ -n "$test" && -n "$reason" ]] \
    || fail "malformed row in $allowed_ignore_manifest"
  [[ -n "${target_exists[$target]+set}" ]] \
    || fail "unknown target '$target' in $allowed_ignore_manifest"
  key="$target/$test"
  [[ -z "${allowed_ignores[$key]+set}" ]] \
    || fail "duplicate allowed-ignore row for $key"
  [[ -z "$previous_key" || "$previous_key" < "$key" ]] \
    || fail "$allowed_ignore_manifest must remain sorted"
  allowed_ignores["$key"]="$reason"
  previous_key="$key"
done < "$allowed_ignore_manifest"

# These targets exercise the CLI or record/replay engine, not `hermit run`'s
# Detcore unsupported-syscall policy.
declare -A mode_na_targets=(
  [cli]=1
  [record_replay]=1
)
declare -A mode_na_tests=(
  [arbitrary_binaries/record_replay_stable_arbitrary_binaries]="record/replay path"
)

declare -A seen_tests=()
declare -A seen_ignores=()
passed=0
ignored=0
mode_na=0

export HERMIT_FAIL_CLOSED=1
unset HERMIT_BACKEND || true

for target in "${targets[@]}"; do
  printf '\n==> Inventorying %s\n' "$target"
  list_output=$(run_cargo test -p hermit --test "$target" "${cargo_args[@]}" -- --list)
  mapfile -t tests < <(printf '%s\n' "$list_output" | sed -n 's/: test$//p')

  if [[ -n "${mode_na_targets[$target]+set}" ]]; then
    mode_na=$((mode_na + ${#tests[@]}))
    printf '    mode N/A (%d tests)\n' "${#tests[@]}"
    continue
  fi

  ignored_output=$(run_cargo test -p hermit --test "$target" "${cargo_args[@]}" -- --list --ignored)
  declare -A target_ignored=()
  while IFS= read -r test; do
    [[ -z "$test" ]] && continue
    key="$target/$test"
    target_ignored["$key"]=1
    [[ -n "${allowed_ignores[$key]+set}" ]] \
      || fail "$key is ignored but is not in $allowed_ignore_manifest"
    seen_ignores["$key"]=1
  done < <(printf '%s\n' "$ignored_output" | sed -n 's/: test$//p')

  for test in "${tests[@]}"; do
    key="$target/$test"
    seen_tests["$key"]=1

    if [[ -n "${allowed_ignores[$key]+set}" && -z "${target_ignored[$key]+set}" ]]; then
      fail "$key is allowlisted as ignored but is now active; remove its ignore row"
    fi
    if [[ -n "${target_ignored[$key]+set}" ]]; then
      ignored=$((ignored + 1))
      continue
    fi
    if [[ -n "${mode_na_tests[$key]+set}" ]]; then
      mode_na=$((mode_na + 1))
      continue
    fi
    if [[ -n "${known_failures[$key]+set}" ]]; then
      continue
    fi

    printf '\n==> Fail-closed: %s\n' "$key"
    run_cargo test -p hermit --test "$target" "${cargo_args[@]}" "$test" \
      -- --exact --test-threads=1
    passed=$((passed + 1))
  done
done

for key in "${!known_failures[@]}"; do
  [[ -n "${seen_tests[$key]+set}" ]] \
    || fail "stale known-failure row for missing test $key"
done
for key in "${!allowed_ignores[@]}"; do
  [[ -n "${seen_ignores[$key]+set}" ]] \
    || fail "stale allowed-ignore row for $key"
done
for key in "${!mode_na_tests[@]}"; do
  [[ -n "${seen_tests[$key]+set}" ]] \
    || fail "stale mode-N/A exception for $key"
done

printf '\nFail-closed ratchet passed: %d enabled, %d known failures, %d ignored, %d mode N/A.\n' \
  "$passed" "${#known_failures[@]}" "$ignored" "$mode_na"
