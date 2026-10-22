# shellcheck shell=bash
# Shared setup for the shell demos (01 to 04).
#
# Source this from a demo's run.sh; do not execute it directly. It checks the
# prerequisites, builds the two small guest programs the demos run, and defines
# the wrappers the demos share. Every wrapper runs the `hermit` found on PATH;
# see demos/README.md for how to build it and put it there.
#
# The wrappers pass --no-virtualize-cpuid so the demos also run on hosts
# without CPUID faulting. CPUID is therefore a host input in these commands.
# Demo 1's run_hermit keeps PMU-based preemption on (see run_hermit below);
# the chaos wrapper turns it off and schedules at system-call boundaries.

set -euo pipefail

DEMOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="$(cd "$DEMOS_DIR/.." && pwd)"
export HERMIT_REPO="${HERMIT_REPO:-$ROOT}"

# Collect every missing prerequisite and report them together, so a fresh
# machine sees the complete list in one run.
# shellcheck source=demos/lib/preflight.sh
. "$DEMOS_DIR/lib/preflight.sh"

preflight_require_file "$HERMIT_REPO/Cargo.toml" \
  "the Hermit source tree is unavailable at $HERMIT_REPO"
preflight_require_command hermit \
  "hermit is not on PATH -- build it with 'make -C $ROOT release-core' and run: export PATH=\"$ROOT/target/release:\$PATH\""
preflight_require_command cargo \
  "cargo is required to build the demo guest programs -- install the toolchain named in rust-toolchain.toml"
preflight_require_command cc \
  "a C compiler is required by the guest programs' build scripts -- install gcc"
preflight_report "${DEMO_LABEL:-demo prerequisites}"

# Tell the reader which Hermit is about to run, and warn when it was built from
# a different commit than this checkout: the walkthrough text describes the
# checkout, not whatever binary happens to be first on PATH.
demo_check_hermit_version() {
  local version head
  version="$(hermit --version 2>/dev/null || true)"
  printf 'Using %s (%s)\n' "${version:-hermit (version unavailable)}" "$(command -v hermit)"
  head="$(git -C "$HERMIT_REPO" rev-parse --short=12 HEAD 2>/dev/null || true)"
  if [ -n "$head" ] && [ -n "$version" ] && [[ "$version" != *"g$head"* ]]; then
    printf 'WARNING: that hermit was not built from this checkout (HEAD %s); rebuild it if results differ from the walkthrough.\n' "$head" >&2
  fi
}
demo_check_hermit_version

# Build the guest programs the demos run. They stay debug builds so demo 4's
# analyzer can resolve their source locations. Set DEMO_SKIP_BUILD=1 to reuse
# an existing build.
if [ "${DEMO_SKIP_BUILD:-0}" != "1" ]; then
  cargo build --locked --manifest-path "$HERMIT_REPO/Cargo.toml" \
    -p hermetic_infra_hermit_flaky-tests --bin hello_race
  cargo build --locked --manifest-path "$HERMIT_REPO/Cargo.toml" \
    -p hermetic_infra_hermit_tests --bin rustbin_heap_ptrs
fi

CARGO_TARGET="${CARGO_TARGET_DIR:-$HERMIT_REPO/target}"
export HELLO_RACE="${HELLO_RACE:-$CARGO_TARGET/debug/hello_race}"
export HEAP_PTRS="${HEAP_PTRS:-$CARGO_TARGET/debug/rustbin_heap_ptrs}"
export RACE_SH="${RACE_SH:-$HERMIT_REPO/examples/race.sh}"

for guest in "$HELLO_RACE" "$HEAP_PTRS"; do
  test -x "$guest" || { echo "missing guest program: $guest (unset DEMO_SKIP_BUILD to build it)" >&2; exit 1; }
done

# `hermit run` mounts a private tmpfs over /tmp, so the guest does not see the
# host's /tmp. When this checkout lives under /tmp, that hides the guest
# programs and scripts, and hermit fails with "Could not execute ... No such
# file or directory". In that case, bind the real /tmp into the guest with
# --tmp=/tmp. HERMIT_ANALYZE_TMP_FLAGS passes the same option to the runs that
# `hermit analyze` starts (demo 4).
HERMIT_TMP_FLAGS=()
# shellcheck disable=SC2034  # used by demo 4 after sourcing this file
HERMIT_ANALYZE_TMP_FLAGS=()
case "$HERMIT_REPO/" in
  /tmp/*)
    HERMIT_TMP_FLAGS=(--tmp=/tmp)
    # shellcheck disable=SC2034  # used by demo 4 after sourcing this file
    HERMIT_ANALYZE_TMP_FLAGS=(--run-arg=--tmp=/tmp)
    ;;
esac

# Per-run scratch directory, plus an ignored artifact directory under target/.
export DEMO_TMP="${DEMO_TMP:-$(mktemp -d -t hermit-demo.XXXXXX)}"
export DEMO_ARTIFACTS="${DEMO_ARTIFACTS:-$CARGO_TARGET/demos/${DEMO_TMP##*/}}"
mkdir -p "$DEMO_TMP" "$DEMO_ARTIFACTS"

# Run wrapper: minimal environment, CPUID virtualization off, PMU-based
# preemption on. Many-threaded guests such as python3 need preemption: without
# it a thread that spins in user space yields only at system calls, and the
# run can stall for minutes. With PMU preemption each run takes about a second.
# On a host without user-accessible performance counters, set
# HERMIT_DEMO_MAX_TIMESLICE=disabled (the python3 step may then be slow).
HERMIT_PREEMPTION_FLAGS=()
if [ -n "${HERMIT_DEMO_MAX_TIMESLICE:-}" ]; then
  HERMIT_PREEMPTION_FLAGS=(--max-timeslice="$HERMIT_DEMO_MAX_TIMESLICE")
fi
run_hermit() {
  hermit --log=error run \
    "${HERMIT_TMP_FLAGS[@]}" \
    "${HERMIT_PREEMPTION_FLAGS[@]}" \
    --base-env=minimal \
    --no-virtualize-cpuid \
    "$@"
}

# Verify wrapper. It differs from run_hermit in two ways:
#   1. --log=info. --verify compares Hermit's deterministic execution log, and
#      at --log=error that log is empty, so the comparison would prove nothing.
#   2. PMU preemption stays at its default. The racy guest in demo 1 is only
#      reliably determinized with it, so this step needs performance counters.
verify_hermit() {
  hermit --log=info run --verify --no-virtualize-cpuid "${HERMIT_TMP_FLAGS[@]}" "$@"
}

# Chaos wrapper: a seeded scheduler that deliberately varies thread order.
chaos_run() {
  local seed="$1"
  hermit --log=error run \
    "${HERMIT_TMP_FLAGS[@]}" \
    --chaos \
    --seed="$seed" \
    --base-env=minimal \
    --no-virtualize-cpuid \
    --max-timeslice=disabled \
    --env=HERMIT_MODE=chaos \
    -- "$HELLO_RACE"
}

demo_banner() {
  printf '\n=== %s ===\n' "$*"
}

# Succeeds when the host can virtualize CPUID (the CPU supports CPUID
# faulting). Without it CPUID is a host input; direct `hermit` invocations
# such as demo 4's analyze use this to decide whether to pass
# --no-virtualize-cpuid. Every error Reverie prints when it cannot turn on
# CPUID interception ends with "continuing without CPUID interception"
# (reverie-ptrace/src/task.rs); older builds printed "Underlying hardware does
# not support CPUID faulting" instead.
hermit_supports_cpuid_faulting() {
  local out
  out="$(hermit --log=error run --base-env=minimal -- /bin/true 2>&1 || true)"
  [[ "$out" != *"continuing without CPUID interception"* &&
    "$out" != *"does not support CPUID faulting"* ]]
}

# Pass/fail verdict. The demo sets DEMO_LABEL before sourcing this file; the
# ERR trap fires on the first failing command, and demo_success prints on a
# clean finish. run-all.sh reads these lines.
demo_success() { printf '\n=== %s: SUCCESS ===\n' "${DEMO_LABEL:-demo}"; }
demo_failure() {
  local rc=$?
  printf '\n=== %s: FAILURE (exit %d) -- see errors above ===\n' "${DEMO_LABEL:-demo}" "$rc" >&2
}
trap demo_failure ERR
