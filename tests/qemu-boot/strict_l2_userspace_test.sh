#!/usr/bin/env bash

# Boot a QEMU-emulated Linux VM under Hermit and run a real *userspace* program
# inside the deterministic guest, one rung past strict_l2_test.sh (which only
# runs a single freestanding init). A freestanding launcher init
# (qemu_exec_init.c) fork()/execve()s a target program, wait4()s it, prints the
# captured exit status, and powers off. Two scenarios are exercised, each proven
# at L2 (hermit run --strict --verify, bitwise-identical repeat run):
#
#   hello   : a statically linked glibc program (qemu_hello.c) -> exit 7
#   busybox : the host's static busybox running `sh -c 'echo ...; exit 5'`
#
# Select a subset with USERSPACE_SCENARIOS="hello" (space-separated); default is
# "hello busybox". Point BUSYBOX_BIN at a *static* busybox (default
# /usr/sbin/busybox); the busybox scenario is skipped with a warning if none is
# available. Backend: ptrace (default). Relaxations: none (strict).

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
output_dir=${OUTPUT_DIR:-$repo_root/target/qemu-strict-l2-userspace}
phase_timeout_seconds=${QEMU_L2_PHASE_TIMEOUT_SECONDS:-360}
qemu_bin=${QEMU_BIN:-}
# The host glibc is built for the x86-64-v2 baseline and its IFUNC resolvers
# select SSSE3/SSE4.1/SSE4.2 string+startup routines (e.g. `pmaxud` in
# __tls_init_tp, `palignr`+`pcmpistri` in __strcmp_sse42). QEMU's default
# `qemu64` model only advertises up to SSE3, so a static glibc program SIGILLs at
# exec. Enable exactly the x86-64-v2 feature set on the proven fast qemu64 base
# -- and nothing above it (no AVX, no RDRAND) so no new nondeterminism source is
# introduced. TCG treats each SSE flag independently, so all must be listed.
qemu_cpu=${QEMU_CPU:-qemu64,+ssse3,+sse4.1,+sse4.2,+popcnt}
busybox_bin=${BUSYBOX_BIN:-/usr/sbin/busybox}
scenarios=${USERSPACE_SCENARIOS:-hello busybox}
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

for command in cpio gcc gzip grep mktemp setsid sleep tail find sort; do
  command -v "$command" >/dev/null || fail "required command not found: $command"
done

mkdir -p "$output_dir"
launcher_source=$repo_root/tests/shared-futex-verify/qemu_exec_init.c
hello_source=$repo_root/tests/shared-futex-verify/qemu_hello.c
[[ -r $launcher_source ]] || fail "launcher source is not readable: $launcher_source"
[[ -r $hello_source ]] || fail "hello source is not readable: $hello_source"

guest_command_for() {
  local initramfs_image=$1
  guest_command=(
    "$qemu_bin"
    -nodefaults
    -nic none
    -m 256M
    -accel 'tcg,thread=single'
    -smp 1
    -icount 'shift=0,sleep=off'
    -rtc 'base=utc,clock=vm'
    -cpu "$qemu_cpu"
    -kernel "$kernel_image"
    -initrd "$initramfs_image"
    -display none
    -serial stdio
    -monitor none
    -no-reboot
    -append 'console=ttyS0 panic=-1 rdinit=/init'
  )
}

# run_scenario <name> <initramfs_image> <marker> [<marker> ...]
# Boots the guest once under strict mode asserting every marker, then reruns the
# exact command under --verify for the L2 comparison.
run_scenario() {
  local name=$1
  local initramfs_image=$2
  shift 2
  local markers=("$@")

  local run_dir
  run_dir=$(mktemp -d "$output_dir/run.$name.XXXXXX")
  local boot_stdout=$run_dir/boot.stdout
  local boot_stderr=$run_dir/boot.stderr
  local verifier_stdout=$run_dir/verifier.stdout
  local verifier_stderr=$run_dir/verifier.stderr

  guest_command_for "$initramfs_image"
  local boot_command=("$hermit_bin" --log info run --strict -- "${guest_command[@]}")
  local verify_command=("$hermit_bin" --log info run --strict --verify -- "${guest_command[@]}")

  printf '\n=== scenario: %s ===\n' "$name"
  printf 'kernel=%s\ninitramfs=%s\nartifacts=%s\n' \
    "$kernel_image" "$initramfs_image" "$run_dir"

  # ---- strict boot oracle ----
  setsid --wait "${boot_command[@]}" >"$boot_stdout" 2>"$boot_stderr" &
  active_pid=$!
  active_pgid=$active_pid
  wait_for_session_group
  local deadline=$((SECONDS + phase_timeout_seconds))
  while process_alive || group_alive; do
    if ((SECONDS >= deadline)); then
      stop_active_group
      cat "$boot_stdout"
      tail -200 "$boot_stderr" >&2
      fail "[$name] strict boot-oracle phase exceeded ${phase_timeout_seconds}s"
    fi
    sleep 1
  done

  set +e
  wait "$active_pid"
  local status=$?
  set -e
  cat "$boot_stdout"
  if ((status != 0)); then
    tail -200 "$boot_stderr" >&2
    fail "[$name] strict boot oracle exited with status $status"
  fi

  local marker
  for marker in "${markers[@]}"; do
    if ! grep -Fq "$marker" "$boot_stdout"; then
      tail -200 "$boot_stderr" >&2
      fail "[$name] strict boot oracle exited without marker: $marker"
    fi
  done
  local clock_failures='Unable to calibrate against PIT|Clocksource .* skewed|Marking TSC unstable|No current clocksource'
  if grep -Eq "$clock_failures" "$boot_stdout"; then
    tail -200 "$boot_stderr" >&2
    fail "[$name] strict boot oracle reached a rejected clock failure"
  fi
  stop_active_group

  # ---- L2 verify ----
  : >"$verifier_stdout"
  : >"$verifier_stderr"
  setsid --wait "${verify_command[@]}" >"$verifier_stdout" 2>"$verifier_stderr" &
  active_pid=$!
  active_pgid=$active_pid
  wait_for_session_group
  local phase=run1
  deadline=$((SECONDS + phase_timeout_seconds))
  local saw_run2=0 saw_compare=0
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
      fail "[$name] strict L2 phase '$phase' exceeded ${phase_timeout_seconds}s"
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
    fail "[$name] strict L2 verification exited with status $status"
  fi
  grep -Fq ':: Success: deterministic. Determinism verified.' "$verifier_stderr" || \
    fail "[$name] Hermit exited successfully without the L2 verification marker"
  stop_active_group

  printf '[%s] QEMU strict L2 userspace program passed.\n' "$name"
}

build_hello_initramfs() {
  local run_dir root init image
  run_dir=$(mktemp -d "$output_dir/build.hello.XXXXXX")
  root=$run_dir/root
  image=$run_dir/initramfs.cpio.gz
  mkdir -p "$root"
  # Freestanding launcher (default scenario execs /hello).
  gcc -Os -nostdlib -static -fno-stack-protector -fno-pie -no-pie \
    "$launcher_source" -o "$root/init"
  # Statically linked glibc hello program.
  gcc -O2 -static -fno-pie -no-pie "$hello_source" -o "$root/hello"
  (
    cd "$root"
    find . -print0 | sort -z | cpio --quiet --null -o -H newc
  ) | gzip -9 >"$image"
  printf '%s' "$image"
}

build_busybox_initramfs() {
  local run_dir root image
  run_dir=$(mktemp -d "$output_dir/build.busybox.XXXXXX")
  root=$run_dir/root
  image=$run_dir/initramfs.cpio.gz
  mkdir -p "$root/bin"
  # Freestanding launcher built for the busybox scenario (execs /bin/busybox).
  gcc -Os -nostdlib -static -fno-stack-protector -fno-pie -no-pie \
    -DSCENARIO_BUSYBOX "$launcher_source" -o "$root/init"
  cp "$busybox_bin" "$root/bin/busybox"
  chmod +x "$root/bin/busybox"
  (
    cd "$root"
    find . -print0 | sort -z | cpio --quiet --null -o -H newc
  ) | gzip -9 >"$image"
  printf '%s' "$image"
}

ran_any=0
for scenario in $scenarios; do
  case "$scenario" in
    hello)
      image=$(build_hello_initramfs)
      run_scenario hello "$image" \
        SHARED_FUTEX_QEMU_KERNEL_OK \
        'QEMU_USERSPACE_LAUNCH prog=/hello' \
        'QEMU_USERSPACE_HELLO_OK pid=' \
        'QEMU_USERSPACE_EXIT prog=hello exited=1 status=7' \
        QEMU_USERSPACE_DONE
      ran_any=1
      ;;
    busybox)
      if [[ ! -r $busybox_bin ]]; then
        printf 'warning: busybox not found at %s; skipping busybox scenario\n' \
          "$busybox_bin" >&2
        continue
      fi
      if file "$busybox_bin" 2>/dev/null | grep -q 'dynamically linked'; then
        printf 'warning: %s is dynamically linked; skipping (need a static busybox)\n' \
          "$busybox_bin" >&2
        continue
      fi
      image=$(build_busybox_initramfs)
      run_scenario busybox "$image" \
        SHARED_FUTEX_QEMU_KERNEL_OK \
        'QEMU_USERSPACE_LAUNCH prog=/bin/busybox' \
        'QEMU_BUSYBOX_HELLO from Linux' \
        'QEMU_USERSPACE_EXIT prog=busybox-sh exited=1 status=5' \
        QEMU_USERSPACE_DONE
      ran_any=1
      ;;
    *)
      fail "unknown scenario: $scenario (expected: hello busybox)"
      ;;
  esac
done

((ran_any == 1)) || fail "no scenarios ran"
printf '\nAll QEMU strict L2 userspace scenarios passed.\n'
