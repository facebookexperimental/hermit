#!/usr/bin/env bash

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

process_alive() {
  [[ -n ${active_pid:-} ]] && kill -0 "$active_pid" 2>/dev/null
}

group_alive() {
  [[ -n ${active_pgid:-} ]] && kill -0 -- "-$active_pgid" 2>/dev/null
}

signal_active_session() {
  local signal=$1

  if group_alive; then
    kill "-$signal" -- "-$active_pgid" 2>/dev/null || true
  elif process_alive; then
    kill "-$signal" "$active_pid" 2>/dev/null || true
  fi
}

stop_active_group() {
  if ! group_alive && ! process_alive; then
    active_pid=""
    active_pgid=""
    return
  fi

  signal_active_session TERM
  sleep 2
  signal_active_session KILL
  if [[ -n ${active_pid:-} ]]; then
    wait "$active_pid" 2>/dev/null || true
  fi
  active_pid=""
  active_pgid=""
}

wait_for_session_group() {
  local startup_deadline=$((SECONDS + 5))

  while process_alive && ! group_alive; do
    if ((SECONDS >= startup_deadline)); then
      stop_active_group
      fail "setsid did not establish the QEMU process group within 5s"
    fi
    sleep 0.01
  done
}

cleanup() {
  stop_active_group
}
trap cleanup EXIT
trap 'exit 130' INT TERM

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)
hermit_bin=${HERMIT_BIN:-$repo_root/target/release/hermit}
kernel_image=${KERNEL_IMAGE:-/boot/vmlinuz}
output_dir=${OUTPUT_DIR:-$repo_root/target/qemu-strict-l2}
phase_timeout_seconds=${QEMU_L2_PHASE_TIMEOUT_SECONDS:-300}
qemu_bin=${QEMU_BIN:-}
active_pid=""
active_pgid=""

if [[ -z $qemu_bin ]]; then
  qemu_bin=$(command -v qemu-system-x86_64 || true)
fi

[[ -x $hermit_bin ]] || fail "Hermit release binary not found: $hermit_bin"
[[ -n $qemu_bin && -x $qemu_bin ]] || fail "qemu-system-x86_64 not found; set QEMU_BIN"
[[ -r $kernel_image ]] || fail "kernel image is not readable: $kernel_image"
if [[ ! $phase_timeout_seconds =~ ^[1-9][0-9]*$ ]]; then
  fail "QEMU_L2_PHASE_TIMEOUT_SECONDS must be a positive integer"
fi

for command in cpio gcc gzip grep mktemp setsid sleep tail; do
  command -v "$command" >/dev/null || fail "required command not found: $command"
done

mkdir -p "$output_dir"
run_dir=$(mktemp -d "$output_dir/run.XXXXXX")
initramfs_image=${INITRAMFS_IMAGE:-$run_dir/initramfs.cpio.gz}
init_source=$repo_root/tests/shared-futex-verify/qemu_init.c
initramfs_root=$run_dir/initramfs-root
boot_stdout=$run_dir/boot.stdout
boot_stderr=$run_dir/boot.stderr
verifier_stdout=$run_dir/verifier.stdout
verifier_stderr=$run_dir/verifier.stderr
[[ -r $init_source ]] || fail "init source is not readable: $init_source"
mkdir -p "$initramfs_root" "$(dirname -- "$initramfs_image")"

gcc -Os -nostdlib -static -fno-stack-protector -fno-pie -no-pie \
  "$init_source" \
  -o "$initramfs_root/init"
(
  cd "$initramfs_root"
  printf '.\n./init\n' | cpio --quiet -o -H newc
) | gzip -9 >"$initramfs_image"

printf 'kernel=%s\ninitramfs=%s\nartifacts=%s\n' \
  "$kernel_image" "$initramfs_image" "$run_dir"

# AUTONOMOUS-BOT-IMPLEMENTED
# TODO-HUMAN-REVIEW(#553)
# The ptrace verifier captures guest stdout without replaying it. Run the exact
# guest once under strict mode to assert the init success marker, then run the
# same command under --verify for the L2 comparison.
guest_command=(
  "$qemu_bin"
  -nodefaults
  -nic none
  -m 256M
  -accel 'tcg,thread=single'
  -smp 1
  -icount 'shift=0,sleep=off'
  -rtc 'base=utc,clock=vm'
  -kernel "$kernel_image"
  -initrd "$initramfs_image"
  -display none
  -serial stdio
  -monitor none
  -no-reboot
  -append 'console=ttyS0 panic=-1 rdinit=/init'
)
boot_command=("$hermit_bin" --log info run --strict -- "${guest_command[@]}")
verify_command=("$hermit_bin" --log info run --strict --verify -- "${guest_command[@]}")

setsid --wait "${boot_command[@]}" >"$boot_stdout" 2>"$boot_stderr" &
active_pid=$!
active_pgid=$active_pid
wait_for_session_group
deadline=$((SECONDS + phase_timeout_seconds))
while process_alive || group_alive; do
  if ((SECONDS >= deadline)); then
    stop_active_group
    cat "$boot_stdout"
    tail -200 "$boot_stderr" >&2
    fail "QEMU strict boot-oracle phase exceeded ${phase_timeout_seconds}s"
  fi
  sleep 1
done

set +e
wait "$active_pid"
status=$?
set -e
cat "$boot_stdout"
if ((status != 0)); then
  tail -200 "$boot_stderr" >&2
  fail "QEMU strict boot oracle exited with status $status"
fi

marker=SHARED_FUTEX_QEMU_KERNEL_OK
if ! grep -Fq "$marker" "$boot_stdout"; then
  tail -200 "$boot_stderr" >&2
  fail "QEMU strict boot oracle exited without marker $marker"
fi
clock_failures='Unable to calibrate against PIT|Clocksource .* skewed|Marking TSC unstable|No current clocksource'
if grep -Eq "$clock_failures" "$boot_stdout"; then
  tail -200 "$boot_stderr" >&2
  fail "QEMU strict boot oracle reached a rejected clock failure"
fi
stop_active_group

: >"$verifier_stdout"
: >"$verifier_stderr"
setsid --wait "${verify_command[@]}" >"$verifier_stdout" 2>"$verifier_stderr" &
active_pid=$!
active_pgid=$active_pid
wait_for_session_group
phase=run1
deadline=$((SECONDS + phase_timeout_seconds))
saw_run2=0
saw_compare=0

while process_alive || group_alive; do
  if ((saw_run2 == 0)) && grep -Fq ':: Run2...' "$verifier_stderr"; then
    phase=run2
    deadline=$((SECONDS + phase_timeout_seconds))
    saw_run2=1
  elif ((saw_compare == 0)) && grep -Fq ':: Comparing logs...' "$verifier_stderr"; then
    phase=compare
    deadline=$((SECONDS + phase_timeout_seconds))
    saw_compare=1
  fi

  if ((SECONDS >= deadline)); then
    stop_active_group
    cat "$verifier_stdout"
    cat "$verifier_stderr" >&2
    fail "QEMU strict L2 phase '$phase' exceeded ${phase_timeout_seconds}s"
  fi
  sleep 1
done

set +e
wait "$active_pid"
status=$?
set -e

cat "$verifier_stdout"
cat "$verifier_stderr" >&2
if ((status != 0)); then
  fail "QEMU strict L2 verification exited with status $status"
fi
grep -Fq ':: Success: deterministic. Determinism verified.' "$verifier_stderr" || \
  fail "Hermit exited successfully without the L2 verification marker"
stop_active_group

printf 'QEMU strict L2 boot passed.\n'
