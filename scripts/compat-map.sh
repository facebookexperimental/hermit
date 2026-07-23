#!/usr/bin/env bash
#
# compat-map.sh — quick Hermit compatibility / frontier status at a glance.
#
# Answers "where are we?" without running the full stress/envelope suite:
#   * which instrumentation backends are usable on this host (ptrace/kvm/dbi),
#   * pass/fail counts for a small set of system binaries at assurance
#     levels L1..L3, on the ptrace backend, and
#   * an inventory of the rr-test and OSS-app buckets (counted, not executed,
#     because those download/build or take minutes).
#
# It is intentionally FAST (a handful of tiny guest runs) so it can be run
# on-demand. For the authoritative determinism envelope use ./validate.sh.
#
# Output: a human-readable summary by default; machine-readable JSON with
# --json (and/or written to a file with --out FILE).
#
# The assurance probes deliberately mirror validate.sh's host-capability-matched
# flags (NOT full --strict): --base-env=minimal --no-virtualize-cpuid
# --preemption-timeout=disabled. This runs on hosts without PMU access or CPUID
# faulting, so the counts are comparable to the working-envelope rubric.
#
#   L1  deterministic run            run ... -- PROG
#   L2  bitwise-identical repeat     run ... --verify -- PROG
#   L3  memory determinism           run ... --verify --detlog-heap --detlog-stack -- PROG
#   L4  stress-hardened              not measured here (see ./validate.sh)

set -uo pipefail

# ---------------------------------------------------------------------------
# Configuration and CLI
# ---------------------------------------------------------------------------

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)

HERMIT_BIN=${HERMIT_BIN:-"$repo_root/target/debug/hermit"}
CASE_TIMEOUT=${CASE_TIMEOUT_SECONDS:-30}
emit_json=0
json_out=""
run_rr=0

usage() {
    cat <<'EOF'
Usage: scripts/compat-map.sh [options]

Options:
  --json            Print the machine-readable JSON report to stdout
                    (instead of the human summary).
  --out FILE        Also write the JSON report to FILE.
  --run-rr          Additionally execute the rr suite (slow; off by default).
  -h, --help        Show this help.

Environment:
  HERMIT_BIN               Path to the hermit binary (default: target/debug/hermit).
  CASE_TIMEOUT_SECONDS     Per-guest-run timeout in seconds (default: 30).
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --json) emit_json=1 ;;
        --out) shift; json_out=${1:-} ;;
        --run-rr) run_rr=1 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'error: unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

log() { [[ $emit_json -eq 1 ]] || printf '%s\n' "$*" >&2; }

if [[ ! -x $HERMIT_BIN ]]; then
    printf 'error: hermit binary not found or not executable: %s\n' "$HERMIT_BIN" >&2
    # shellcheck disable=SC2016  # backticks are a literal hint, not a command substitution
    printf 'hint: build it first with `cargo build -p hermit --bin hermit`, or set HERMIT_BIN.\n' >&2
    exit 1
fi

# Host-matched run flags shared by every assurance level (see header).
declare -ar RUN_FLAGS=(run --base-env=minimal --no-virtualize-cpuid --preemption-timeout=disabled)

# System binaries probed on the ptrace backend, each with a stable,
# side-effect-free, exit-0 invocation (filter tools use a version probe, matching
# the arbitrary-binary launch matrix). Absent binaries are skipped silently.
declare -ar SYSTEM_BINS=(true echo ls cat head wc sort grep sed awk)
declare -rA BIN_ARGS=(
    [true]=""
    [echo]="hi"
    [ls]="/"
    [cat]="/dev/null"
    [head]="/dev/null"
    [wc]="/dev/null"
    [sort]="/dev/null"
    [grep]="--version"
    [sed]="--version"
    [awk]="--version"
)

# ---------------------------------------------------------------------------
# JSON helpers (values here are controlled: integers and known identifiers)
# ---------------------------------------------------------------------------

json_str() { # escape a string for embedding in JSON
    local s=${1//\\/\\\\}
    s=${s//\"/\\\"}
    # Collapse any embedded tabs/newlines/carriage returns to spaces so the
    # output is always valid single-line JSON.
    s=${s//$'\t'/ }
    s=${s//$'\n'/ }
    s=${s//$'\r'/ }
    printf '"%s"' "$s"
}

# ---------------------------------------------------------------------------
# Host / revision metadata
# ---------------------------------------------------------------------------

host_kernel=$(uname -sr 2>/dev/null || echo unknown)
host_arch=$(uname -m 2>/dev/null || echo unknown)
# systemd-detect-virt exits non-zero when it reports "none" (bare metal), so
# capture its output directly rather than treating that exit as an error.
host_virt=$(systemd-detect-virt 2>/dev/null)
[[ -n $host_virt ]] || host_virt="unknown"
host_cpu=$(grep -m1 '^model name' /proc/cpuinfo 2>/dev/null | sed 's/^model name[[:space:]]*:[[:space:]]*//' || echo unknown)
perf_paranoid=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo unknown)

hermit_rev=$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null || echo unknown)
hermit_branch=$(git -C "$repo_root" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)
reverie_rev=$(grep -oE 'reverie[^#]*#([0-9a-f]{7,40})' "$repo_root/Cargo.lock" 2>/dev/null \
    | grep -oE '[0-9a-f]{7,40}' | head -1 || true)
[[ -n ${reverie_rev:-} ]] || reverie_rev="unknown"

# ---------------------------------------------------------------------------
# Backend detection
# ---------------------------------------------------------------------------

backend_flag_supported=0
if "$HERMIT_BIN" run --help 2>&1 | grep -q -- '--backend'; then
    backend_flag_supported=1
fi

# ptrace is the default/production backend and is always attempted.
ptrace_available=1

# kvm needs a usable /dev/kvm; the backend only runs a builtin hello-world probe.
kvm_available=0
kvm_reason="no --backend flag"
if [[ $backend_flag_supported -eq 1 ]]; then
    if [[ -r /dev/kvm && -w /dev/kvm ]]; then
        kvm_available=1
        kvm_reason="/dev/kvm readable+writable"
    else
        kvm_reason="/dev/kvm not readable+writable"
    fi
fi

# dbi needs a configured DynamoRIO environment.
dbi_available=0
dbi_reason="no --backend flag"
if [[ $backend_flag_supported -eq 1 ]]; then
    if [[ -n ${DYNAMORIO_HOME:-} && -n ${HERMIT_DRRUN:-} && -n ${HERMIT_DBI_CLIENT:-} ]]; then
        dbi_available=1
        dbi_reason="DynamoRIO environment configured"
    else
        dbi_reason="DynamoRIO env not set (DYNAMORIO_HOME/HERMIT_DRRUN/HERMIT_DBI_CLIENT)"
    fi
fi

# ---------------------------------------------------------------------------
# Assurance probes (ptrace, system-binaries bucket)
# ---------------------------------------------------------------------------

# run_level LEVEL PROG_PATH [GUEST_ARGS...] -> 0 on pass, non-zero on fail
run_level() {
    local level=$1 prog=$2
    shift 2
    local -a extra=()
    case "$level" in
        L1) extra=() ;;
        L2) extra=(--verify) ;;
        L3) extra=(--verify --detlog-heap --detlog-stack) ;;
    esac
    timeout "$CASE_TIMEOUT" "$HERMIT_BIN" "${RUN_FLAGS[@]}" "${extra[@]}" -- "$prog" "$@" \
        </dev/null >/dev/null 2>&1
}

declare -A pass=([L1]=0 [L2]=0 [L3]=0)
declare -A fail=([L1]=0 [L2]=0 [L3]=0)
sysbin_total=0
declare -a sysbin_detail=()   # "name L1 L2 L3" with pass/fail/skip tokens

if [[ $ptrace_available -eq 1 ]]; then
    log "Probing system binaries on the ptrace backend (L1..L3)..."
    for name in "${SYSTEM_BINS[@]}"; do
        prog=$(command -v "$name" 2>/dev/null || true)
        if [[ -z $prog ]]; then
            sysbin_detail+=("$name skip skip skip")
            continue
        fi
        sysbin_total=$((sysbin_total + 1))
        # shellcheck disable=SC2206  # intentional word-splitting of the arg string
        local_args=(${BIN_ARGS[$name]:-})
        row="$name"
        for level in L1 L2 L3; do
            if run_level "$level" "$prog" "${local_args[@]}"; then
                pass[$level]=$(( ${pass[$level]} + 1 ))
                row+=" pass"
            else
                fail[$level]=$(( ${fail[$level]} + 1 ))
                row+=" fail"
            fi
        done
        sysbin_detail+=("$row")
        log "  $row"
    done
fi

# ---------------------------------------------------------------------------
# Backend launch probes (kvm/dbi run only a builtin hello-world)
# ---------------------------------------------------------------------------

kvm_probe="not-run"
if [[ $kvm_available -eq 1 ]]; then
    if timeout "$CASE_TIMEOUT" "$HERMIT_BIN" run --backend kvm -- /bin/true </dev/null >/dev/null 2>&1; then
        kvm_probe="pass"
    else
        kvm_probe="fail"
    fi
fi

dbi_probe="not-run"
if [[ $dbi_available -eq 1 ]]; then
    if timeout "$CASE_TIMEOUT" "$HERMIT_BIN" run --backend dbi -- /bin/true </dev/null >/dev/null 2>&1; then
        dbi_probe="pass"
    else
        dbi_probe="fail"
    fi
fi

# ---------------------------------------------------------------------------
# Bucket inventories (counted, not executed unless requested)
# ---------------------------------------------------------------------------

rr_suite_file="$repo_root/hermit-cli/tests/rr_suite.rs"
rr_count=0
[[ -f $rr_suite_file ]] && rr_count=$(grep -cE 'rr_test!\(' "$rr_suite_file" 2>/dev/null || echo 0)

rr_status="inventory-only"
rr_pass=""
rr_fail=""
if [[ $run_rr -eq 1 && $rr_count -gt 0 ]]; then
    log "Running rr suite (this is slow)..."
    rr_log=$(mktemp)
    ( cd "$repo_root" && timeout 1800 cargo test -p hermit --test rr_suite -- --ignored ) \
        >"$rr_log" 2>&1
    line=$(grep -E 'test result:' "$rr_log" | tail -1)
    rr_pass=$(sed -nE 's/.* ([0-9]+) passed.*/\1/p' <<<"$line")
    rr_fail=$(sed -nE 's/.* ([0-9]+) failed.*/\1/p' <<<"$line")
    rr_status="executed"
    rm -f "$rr_log"
fi

# OSS apps: detect integration tests and experiment evidence dirs.
declare -a oss_apps=()
for t in leveldb redis_strict sqlite_veryquick python_stdlib language_runtime_determinism; do
    [[ -f "$repo_root/hermit-cli/tests/$t.rs" ]] && oss_apps+=("test:$t")
done
if [[ -d "$repo_root/experiments" ]]; then
    for d in "$repo_root"/experiments/*/; do
        base=$(basename "$d")
        case "$base" in
            lulesh*|ninja*|leveldb*|redis*|sqlite*) oss_apps+=("experiment:$base") ;;
        esac
    done
fi
oss_count=${#oss_apps[@]}

# ---------------------------------------------------------------------------
# Emit JSON
# ---------------------------------------------------------------------------

build_json() {
    local d
    printf '{\n'
    printf '  "schema": "hermit-compat-map/v1",\n'
    printf '  "host": {"kernel": %s, "arch": %s, "virt": %s, "cpu": %s, "perf_event_paranoid": %s},\n' \
        "$(json_str "$host_kernel")" "$(json_str "$host_arch")" "$(json_str "$host_virt")" \
        "$(json_str "$host_cpu")" "$(json_str "$perf_paranoid")"
    printf '  "revision": {"hermit": %s, "branch": %s, "reverie": %s},\n' \
        "$(json_str "$hermit_rev")" "$(json_str "$hermit_branch")" "$(json_str "$reverie_rev")"
    printf '  "backends": {\n'
    printf '    "ptrace": {"available": true, "probe": "system-binaries-matrix"},\n'
    printf '    "kvm": {"available": %s, "reason": %s, "probe": %s},\n' \
        "$([[ $kvm_available -eq 1 ]] && echo true || echo false)" "$(json_str "$kvm_reason")" "$(json_str "$kvm_probe")"
    printf '    "dbi": {"available": %s, "reason": %s, "probe": %s}\n' \
        "$([[ $dbi_available -eq 1 ]] && echo true || echo false)" "$(json_str "$dbi_reason")" "$(json_str "$dbi_probe")"
    printf '  },\n'
    printf '  "buckets": {\n'
    printf '    "system_binaries": {\n'
    printf '      "backend": "ptrace", "total": %d,\n' "$sysbin_total"
    printf '      "by_level": {\n'
    printf '        "L1": {"pass": %d, "fail": %d},\n' "${pass[L1]}" "${fail[L1]}"
    printf '        "L2": {"pass": %d, "fail": %d},\n' "${pass[L2]}" "${fail[L2]}"
    printf '        "L3": {"pass": %d, "fail": %d}\n' "${pass[L3]}" "${fail[L3]}"
    printf '      },\n'
    printf '      "detail": ['
    local first=1 entry name l1 l2 l3
    for entry in "${sysbin_detail[@]}"; do
        read -r name l1 l2 l3 <<<"$entry"
        [[ $first -eq 1 ]] || printf ','
        first=0
        printf '\n        {"name": %s, "L1": %s, "L2": %s, "L3": %s}' \
            "$(json_str "$name")" "$(json_str "$l1")" "$(json_str "$l2")" "$(json_str "$l3")"
    done
    printf '\n      ]\n'
    printf '    },\n'
    printf '    "rr_tests": {"status": %s, "case_count": %d' "$(json_str "$rr_status")" "$rr_count"
    if [[ $rr_status == executed ]]; then
        printf ', "passed": %s, "failed": %s' "${rr_pass:-null}" "${rr_fail:-null}"
    fi
    printf '},\n'
    printf '    "oss_apps": {"status": "inventory-only", "count": %d, "items": [' "$oss_count"
    first=1
    for d in "${oss_apps[@]}"; do
        [[ $first -eq 1 ]] || printf ', '
        first=0
        printf '%s' "$(json_str "$d")"
    done
    printf ']}\n'
    printf '  }\n'
    printf '}\n'
}

json=$(build_json)

if [[ -n $json_out ]]; then
    printf '%s\n' "$json" >"$json_out"
    log "Wrote JSON report to $json_out"
fi

if [[ $emit_json -eq 1 ]]; then
    printf '%s\n' "$json"
    exit 0
fi

# ---------------------------------------------------------------------------
# Human-readable summary
# ---------------------------------------------------------------------------

hr() { printf '%s\n' "------------------------------------------------------------"; }

echo
echo "Hermit compatibility map — frontier status at a glance"
hr
printf 'hermit    %s (%s)\n' "$hermit_rev" "$hermit_branch"
printf 'reverie   %s\n' "$reverie_rev"
printf 'host      %s  %s  virt=%s\n' "$host_arch" "$host_kernel" "$host_virt"
printf 'cpu       %s\n' "$host_cpu"
printf 'perf      perf_event_paranoid=%s\n' "$perf_paranoid"
hr
echo "Backends"
printf '  ptrace  available     (arbitrary ELF; used for the matrix below)\n'
printf '  kvm     %-11s %s [probe: %s]\n' \
    "$([[ $kvm_available -eq 1 ]] && echo available || echo unavailable)" "$kvm_reason" "$kvm_probe"
printf '  dbi     %-11s %s [probe: %s]\n' \
    "$([[ $dbi_available -eq 1 ]] && echo available || echo unavailable)" "$dbi_reason" "$dbi_probe"
hr
echo "System binaries (ptrace backend) — pass/total per assurance level"
printf '  L1 deterministic run        %d/%d\n' "${pass[L1]}" "$sysbin_total"
printf '  L2 bitwise-identical repeat %d/%d\n' "${pass[L2]}" "$sysbin_total"
printf '  L3 memory determinism       %d/%d\n' "${pass[L3]}" "$sysbin_total"
printf '  L4 stress-hardened          (not measured here; run ./validate.sh)\n'
if [[ ${#sysbin_detail[@]} -gt 0 ]]; then
    echo "  per-binary (L1 L2 L3):"
    for entry in "${sysbin_detail[@]}"; do
        read -r name l1 l2 l3 <<<"$entry"
        printf '    %-8s %-4s %-4s %-4s\n' "$name" "$l1" "$l2" "$l3"
    done
fi
hr
echo "Other buckets"
if [[ $rr_status == executed ]]; then
    printf '  rr tests   %s: %s passed, %s failed (of %d cases)\n' \
        "$rr_status" "${rr_pass:-?}" "${rr_fail:-?}" "$rr_count"
else
    printf '  rr tests   %d cases catalogued (inventory-only; run with --run-rr or\n' "$rr_count"
    # shellcheck disable=SC2016  # backticks are a literal hint, not a command substitution
    printf '             `cargo test -p hermit --test rr_suite -- --ignored`)\n'
fi
printf '  oss apps   %d catalogued (inventory-only): %s\n' "$oss_count" "${oss_apps[*]:-none}"
hr
echo "Note: assurance probes use host-matched flags (no --strict/PMU); this is a"
echo "quick map, not the full envelope. Use ./validate.sh for L4 and strict gates."
echo
