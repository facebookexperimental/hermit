#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
HERMIT_BIN="${HERMIT_BIN:-${REPO_ROOT}/target/debug/hermit}"
OCAML_SWITCH="${OCAML_SWITCH:-5.3.0}"
NATIVE_RUNS="${NATIVE_RUNS:-40}"
STRICT_RUNS="${STRICT_RUNS:-5}"
WORKERS="${WORKERS:-4}"
ITERATIONS="${ITERATIONS:-1000000}"
RUN_TIMEOUT="${RUN_TIMEOUT:-60s}"

for value in NATIVE_RUNS STRICT_RUNS WORKERS ITERATIONS; do
  if ! [[ "${!value}" =~ ^[1-9][0-9]*$ ]]; then
    printf 'error: %s must be a positive integer\n' "${value}" >&2
    exit 2
  fi
done

if [[ ! -x "${HERMIT_BIN}" ]]; then
  printf 'error: Hermit binary is not executable: %s\n' "${HERMIT_BIN}" >&2
  printf 'build it with: cargo build -p hermit\n' >&2
  exit 2
fi

if ! command -v opam >/dev/null 2>&1; then
  printf 'error: opam is required to select OCaml 5+\n' >&2
  exit 2
fi

if ! opam switch list --short --safe | awk -v target="${OCAML_SWITCH}" '$0 == target { found = 1 } END { exit !found }'; then
  printf 'error: opam switch %s is unavailable\n' "${OCAML_SWITCH}" >&2
  exit 2
fi

if [[ -n "${OUTPUT_ROOT:-}" ]]; then
  ARTIFACT_ROOT="${OUTPUT_ROOT}"
  if [[ -e "${ARTIFACT_ROOT}" ]]; then
    printf 'error: OUTPUT_ROOT already exists: %s\n' "${ARTIFACT_ROOT}" >&2
    exit 2
  fi
  mkdir -p "${ARTIFACT_ROOT}"
else
  ARTIFACT_PARENT="${REPO_ROOT}/target/ocaml-domains"
  mkdir -p "${ARTIFACT_PARENT}"
  ARTIFACT_ROOT="$(mktemp -d "${ARTIFACT_PARENT}/run.XXXXXX")"
  trap 'rm -rf -- "${ARTIFACT_ROOT}"' EXIT
fi

ARTIFACT_ROOT="$(cd -- "${ARTIFACT_ROOT}" && pwd)"
PROGRAM="${ARTIFACT_ROOT}/domain_completion_order"
mkdir -p "${ARTIFACT_ROOT}/native" "${ARTIFACT_ROOT}/strict"
cp "${SCRIPT_DIR}/domain_completion_order.ml" "${ARTIFACT_ROOT}/"
(
  cd "${ARTIFACT_ROOT}"
  opam exec --switch="${OCAML_SWITCH}" -- \
    ocamlopt -o "${PROGRAM}" domain_completion_order.ml
)

OCAML_VERSION="$(opam exec --switch="${OCAML_SWITCH}" -- ocamlopt -version)"
case "${OCAML_VERSION}" in
  5.*) ;;
  *)
    printf 'error: OCaml 5+ is required, found %s\n' "${OCAML_VERSION}" >&2
    exit 2
    ;;
esac

run_native() {
  local iteration="$1"
  timeout --signal=TERM --kill-after=5s "${RUN_TIMEOUT}" \
    "${PROGRAM}" "${WORKERS}" "${ITERATIONS}" \
    >"${ARTIFACT_ROOT}/native/run-${iteration}.stdout" \
    2>"${ARTIFACT_ROOT}/native/run-${iteration}.stderr"
}

run_strict() {
  local iteration="$1"
  timeout --signal=TERM --kill-after=5s "${RUN_TIMEOUT}" \
    "${HERMIT_BIN}" --log off run --strict -- \
    "${PROGRAM}" "${WORKERS}" "${ITERATIONS}" \
    >"${ARTIFACT_ROOT}/strict/run-${iteration}.stdout" \
    2>"${ARTIFACT_ROOT}/strict/run-${iteration}.stderr"
}

for ((iteration = 1; iteration <= NATIVE_RUNS; iteration++)); do
  run_native "${iteration}" || {
    status=$?
    printf 'FAIL: native run %d exited with status %d\n' "${iteration}" "${status}" >&2
    cat "${ARTIFACT_ROOT}/native/run-${iteration}.stderr" >&2
    exit "${status}"
  }
done

native_unique="$(sha256sum "${ARTIFACT_ROOT}"/native/*.stdout | awk '{ print $1 }' | sort -u | wc -l)"
if ((native_unique <= 1)); then
  printf 'FAIL: NONDET_SOURCE=domain scheduling produced one native order in %d runs\n' "${NATIVE_RUNS}" >&2
  exit 1
fi

for ((iteration = 1; iteration <= STRICT_RUNS; iteration++)); do
  run_strict "${iteration}" || {
    status=$?
    printf 'FAIL: strict run %d exited with status %d\n' "${iteration}" "${status}" >&2
    cat "${ARTIFACT_ROOT}/strict/run-${iteration}.stderr" >&2
    exit "${status}"
  }
done

strict_unique="$(sha256sum "${ARTIFACT_ROOT}"/strict/*.stdout | awk '{ print $1 }' | sort -u | wc -l)"
if ((strict_unique != 1)); then
  printf 'FAIL: NONDET_SOURCE=domain scheduling produced %d strict-mode orders in %d runs\n' \
    "${strict_unique}" "${STRICT_RUNS}" >&2
  exit 1
fi

printf 'PASS: OCaml %s domain_completion_order.ml\n' "${OCAML_VERSION}"
printf 'native: %d unique outputs across %d runs\n' "${native_unique}" "${NATIVE_RUNS}"
printf 'strict: %d unique output across %d runs\n' "${strict_unique}" "${STRICT_RUNS}"
printf 'strict sample: '
cat "${ARTIFACT_ROOT}/strict/run-1.stdout"
if [[ -n "${OUTPUT_ROOT:-}" ]]; then
  printf 'artifacts: %s\n' "${ARTIFACT_ROOT}"
fi
