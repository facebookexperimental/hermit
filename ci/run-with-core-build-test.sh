#!/usr/bin/env bash
# Core build entry controls. The first invocation uses the actual source pin;
# fixtures below exercise refusal/width/status/timing mechanisms, not SDK work.
set -euo pipefail
ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
WRAPPER=ci/run-with-core-build.sh
failures=0
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
marker="$work/reached"
rc=0
"./$WRAPPER" /bin/sh -c "printf ok > '$marker'" >"$work/stdout" 2>"$work/stderr" || rc=$?
if ((rc != 0)) || [[ ! -s $marker ]] || ! grep -q 'core-build={pin:' "$work/stderr"; then
    echo "FAIL: actual source pin/width entry did not execute its command and report its binding" >&2
    failures=$((failures+1))
fi
copy="$work/tree"
mkdir -p "$copy/ci"
for script in run-with-core-build.sh run-build-command.sh configure-build-jobs.sh retry-on-compiler-crash.sh; do
    cp -p "$ROOT_DIR/ci/$script" "$copy/ci/$script"
done
cat >"$copy/ci/run-reverie-pin-check.sh" <<'PIN'
#!/bin/sh
printf '%s\n' "${CORE_PIN_FIXTURE:-0000000000000000000000000000000000000000}"
exit "${CORE_PIN_STATUS:-0}"
PIN
chmod +x "$copy/ci/run-reverie-pin-check.sh"
for bad in trailing-text $'0000000000000000000000000000000000000000\nextra'; do
    marker="$work/malformed-$failures"
    rc=0
    CORE_PIN_FIXTURE="$bad" "$copy/$WRAPPER" /bin/sh -c "printf bad > '$marker'" >"$work/out" 2>"$work/err" || rc=$?
    if ((rc != 75)) || [[ -e $marker ]] || ! grep -q 'unidentified Reverie pin' "$work/err"; then
        echo "FAIL: malformed pin must decline75 before command" >&2; failures=$((failures+1))
    fi
done
marker="$work/pin-failed"
rc=0
CORE_PIN_STATUS=2 "$copy/$WRAPPER" /bin/sh -c "printf bad > '$marker'" >"$work/out" 2>"$work/err" || rc=$?
if ((rc != 75)) || [[ -e $marker ]] || ! grep -q 'source pin check failed' "$work/err"; then
    echo "FAIL: failed source check must decline75 before command" >&2; failures=$((failures+1))
fi
for width in 0 invalid; do
    marker="$work/width-$width"
    rc=0
    CARGO_BUILD_JOBS="$width" "$copy/$WRAPPER" /bin/sh -c "printf bad > '$marker'" >"$work/out" 2>"$work/err" || rc=$?
    if ((rc == 0)) || [[ -e $marker ]]; then
        echo "FAIL: invalid/unadmitted width reached command: $width" >&2; failures=$((failures+1))
    fi
done
# The runner contract accepts any positive integer. These commands create no
# Cargo pool; they prove the complete fixture reaches the wrapped command.
for width in 1 32 999999999; do
    marker="$work/positive-width-$width"
    rc=0
    CARGO_BUILD_JOBS="$width" "$copy/$WRAPPER" /bin/sh -c 'printf ok > "$1"' sh "$marker" >"$work/out" 2>"$work/err" || rc=$?
    if ((rc != 0)) || [[ $(cat "$marker" 2>/dev/null) != ok ]]; then
        echo "FAIL: positive runner width did not reach command: $width" >&2; failures=$((failures+1))
    fi
done
# A second Cargo project must retain its own working directory while the core
# wrapper checks the core source and delegates to the unchanged retry owner.
mkdir -p "$work/project"
rc=0
(cd "$work/project" && "$copy/$WRAPPER" /bin/sh -c 'pwd -P > "$1"' sh "$work/cwd") >"$work/out" 2>"$work/err" || rc=$?
if ((rc != 0)) || [[ $(cat "$work/cwd" 2>/dev/null) != "$(cd "$work/project" && pwd -P)" ]]; then
    echo 'FAIL: core build wrapper changed the selected project working directory' >&2; failures=$((failures+1))
fi
expect_exit() {
    local label=$1 want=$2 marker=$3
    shift 3
    local rc=0
    "./$WRAPPER" "$@" >/dev/null 2>&1 || rc=$?
    if ((rc != want)) || { [[ -n $marker ]] && [[ ! -f $marker ]]; }; then
        echo "FAIL: $label status/command execution differs" >&2; failures=$((failures+1))
    fi
}
expect_exit compile-failure 101 "$work/ran101" /bin/sh -c "printf ok > '$work/ran101'; exit 101"
expect_exit generic-failure 1 "" /bin/sh -c 'exit 1'
expect_exit success 0 "$work/ran0" /bin/sh -c "printf ok > '$work/ran0'"
# HERMIT_CARGO_TIMINGS_DIR (diagnostics only): a `cargo build` gets --timings and its
# report is copied into the directory; the build's status is the node's, unchanged;
# unset, or for any other cargo command, nothing changes. A fake cargo first on PATH
# records its argv and writes a report where cargo would.
fakebin="$work/fakebin"
mkdir -p "$fakebin"
cat >"$fakebin/cargo" <<'FAKE'
#!/bin/sh
printf '%s\n' "$*" >>"$FAKE_CARGO_ARGV"
case " $* " in *" --timings "*)
    mkdir -p "$CARGO_TARGET_DIR/cargo-timings"
    printf report >"$CARGO_TARGET_DIR/cargo-timings/cargo-timing-fake.html" ;;
esac
exit "${FAKE_CARGO_STATUS:-0}"
FAKE
chmod +x "$fakebin/cargo"
timings_case() { # timings_case LABEL STATUS TIMINGS_DIR CARGO_ARGS...: run, then echo rc
    local label=$1 status=$2 dir=$3
    shift 3
    rm -rf "${work:?}/target" "${work:?}/argv"
    local rc=0
    PATH="$fakebin:$PATH" FAKE_CARGO_ARGV="$work/argv" FAKE_CARGO_STATUS=$status \
        CARGO_TARGET_DIR="$work/target" HERMIT_CARGO_TIMINGS_DIR=$dir \
        "./$WRAPPER" cargo "$@" >/dev/null 2>&1 || rc=$?
    echo "$rc"
}
for status in 0 101; do
    rm -rf "${work:?}/kept"
    rc=$(timings_case "build" "$status" "$work/kept" build --locked -p x)
    if ((rc != status)); then
        echo "FAIL: a timed cargo build exited $rc, expected its own $status" >&2
        failures=$((failures + 1))
    elif ! grep -qx -- 'build --locked -p x --timings' "$work/argv"; then
        echo "FAIL: a timed cargo build was not given --timings: $(cat "$work/argv")" >&2
        failures=$((failures + 1))
    elif [ "$(cat "$work/kept/cargo-timing-fake.html" 2>/dev/null)" != report ]; then
        echo "FAIL: a timed cargo build's report was not kept in HERMIT_CARGO_TIMINGS_DIR (status $status)" >&2
        failures=$((failures + 1))
    else
        echo "PASS: a timed cargo build (exit $status) gets --timings, keeps its report and its status"
    fi
done
rm -rf "${work:?}/kept"
rc=$(timings_case "unset" 0 "" build --locked -p x)
if ((rc != 0)) || ! grep -qx -- 'build --locked -p x' "$work/argv" || [ -e "$work/kept" ]; then
    echo "FAIL: without HERMIT_CARGO_TIMINGS_DIR the build changed: rc=$rc argv=$(cat "$work/argv" 2>/dev/null)" >&2
    failures=$((failures + 1))
else
    echo "PASS: without HERMIT_CARGO_TIMINGS_DIR a cargo build is unchanged"
fi
rc=$(timings_case "test" 0 "$work/kept" test --no-run)
if ((rc != 0)) || ! grep -qx -- 'test --no-run' "$work/argv" || [ -e "$work/kept" ]; then
    echo "FAIL: HERMIT_CARGO_TIMINGS_DIR changed a cargo command other than build: rc=$rc argv=$(cat "$work/argv" 2>/dev/null)" >&2
    failures=$((failures + 1))
else
    echo "PASS: HERMIT_CARGO_TIMINGS_DIR leaves every other cargo command unchanged"
fi

rc=0
"./$WRAPPER" >/dev/null 2>&1 || rc=$?
if ((rc != 2)); then echo 'FAIL: missing command must exit2' >&2; failures=$((failures+1)); fi
if ((failures != 0)); then echo "core build controls: $failures failure(s)" >&2; exit 1; fi
echo 'core build controls: OK'
