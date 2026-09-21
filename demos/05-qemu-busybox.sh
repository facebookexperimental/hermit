#!/usr/bin/env bash

set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

# Download a URL to a file, portably across open-internet and proxied networks:
# smoke-test a direct connection, download directly when it succeeds, otherwise
# retry through an optional `with-proxy` helper before failing with guidance.
fetch_url() {
  local url="$1" out="$2"
  if curl --fail --location --silent --show-error --head \
       --connect-timeout "${QEMU_FETCH_CONNECT_TIMEOUT:-10}" \
       --max-time "${QEMU_FETCH_PROBE_TIMEOUT:-20}" \
       "$url" -o /dev/null 2>/dev/null; then
    curl --fail --location --silent --show-error "$url" --output "$out"
    return $?
  fi
  if command -v with-proxy >/dev/null 2>&1; then
    printf '  direct connection failed; retrying through with-proxy...\n' >&2
    with-proxy curl --fail --location --silent --show-error \
      "$url" --output "$out"
    return $?
  fi
  fail "cannot reach $url: direct connection failed and no 'with-proxy' helper is on PATH. Provide the kernel locally via KERNEL_IMAGE=/path/to/bzImage, or set http(s)_proxy for your network."
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
hermit_bin=${HERMIT_BIN:-$repo_root/target/release/hermit}
kernel_image=${KERNEL_IMAGE:-}
# Pinned QEMU kernel provisioning (mirrors dev-hermit demos/lib/qemu-assets.sh).
# When KERNEL_IMAGE is unset the demo auto-fetches this exact bzImage into the
# gitignored cache under target/, so it runs out of the box with no manual step.
# Override with QEMU_KERNEL_URL / QEMU_KERNEL_SHA256 to pin a different kernel.
kernel_sha256=${QEMU_KERNEL_SHA256:-e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2}
kernel_url=${QEMU_KERNEL_URL:-https://github.com/rrnewton/dev-hermit/releases/download/qemu-kernel-$kernel_sha256/bzImage}
qemu_bin=${QEMU_BIN:-}
output_dir=${OUTPUT_DIR:-$repo_root/target/qemu-busybox}
timeout_seconds=${DEMO_TIMEOUT_SECONDS:-300}
verify=${VERIFY:-0}
skid_margin=${SKID_MARGIN:-}

if [[ -z $qemu_bin ]]; then
  qemu_bin=$(command -v qemu-system-x86_64 || true)
fi

[[ -x $hermit_bin ]] || fail \
  "Hermit release binary not found: $hermit_bin (run cargo build --release -p hermit --bin hermit)"
[[ -n $qemu_bin && -x $qemu_bin ]] || fail \
  "qemu-system-x86_64 not found; install it or set QEMU_BIN"
[[ $timeout_seconds =~ ^[1-9][0-9]*$ ]] || fail \
  "DEMO_TIMEOUT_SECONDS must be a positive integer"
[[ $verify == 0 || $verify == 1 ]] || fail "VERIFY must be 0 or 1"
[[ -z $skid_margin || $skid_margin =~ ^[1-9][0-9]*$ ]] || fail \
  "SKID_MARGIN must be empty or a positive integer"

for command in grep sha256sum tee timeout; do
  command -v "$command" >/dev/null || fail "$command is required"
done

mkdir -p "$output_dir"

# Resolve the kernel image: honor an explicit KERNEL_IMAGE, otherwise fetch the
# pinned bzImage into the gitignored cache under target/ and verify its sha256
# so the demo works out of the box with no manual provisioning step.
if [[ -z $kernel_image ]]; then
  [[ $kernel_sha256 =~ ^[0-9a-f]{64}$ ]] || fail \
    "QEMU_KERNEL_SHA256 must be a lowercase 64-character SHA-256"
  command -v curl >/dev/null || fail \
    "curl is required to auto-fetch the kernel; install it or set KERNEL_IMAGE"
  kernel_image=$output_dir/bzImage
  cached_kernel_sha=""
  [[ -r $kernel_image ]] && \
    cached_kernel_sha=$(sha256sum "$kernel_image" | cut -d' ' -f1)
  if [[ $cached_kernel_sha != "$kernel_sha256" ]]; then
    [[ -n $cached_kernel_sha ]] && printf \
      'kernel: replacing cache with unexpected sha256 %s\n' \
      "$cached_kernel_sha" >&2
    kernel_tmp=$output_dir/.bzImage.$$
    printf 'Downloading pinned QEMU kernel (%s)...\n' "$kernel_url" >&2
    fetch_url "$kernel_url" "$kernel_tmp" || \
      fail "kernel download failed: $kernel_url"
    downloaded_kernel_sha=$(sha256sum "$kernel_tmp" | cut -d' ' -f1)
    if [[ $downloaded_kernel_sha != "$kernel_sha256" ]]; then
      rm -f "$kernel_tmp"
      fail "kernel sha256 mismatch from $kernel_url: expected $kernel_sha256, got $downloaded_kernel_sha"
    fi
    mv "$kernel_tmp" "$kernel_image"
    printf 'kernel ready: %s\n' "$kernel_image" >&2
  else
    printf 'kernel ready: %s (cached)\n' "$kernel_image" >&2
  fi
fi
[[ -r $kernel_image ]] || fail "kernel image is not readable: $kernel_image"

initramfs_image=${INITRAMFS_IMAGE:-$output_dir/initramfs-busybox.cpio.gz}
console_log=$output_dir/console.log
info_log=$output_dir/hermit-info.log
stderr_log=$output_dir/hermit-stderr.log
verify_json=$output_dir/verify.json

# Preflight the verdict reader BEFORE the boot, which costs several minutes.
# `jq` is the only external dependency VERIFY=1 adds, and it is exactly the
# binary a host BPF FILE_OPEN policy has been observed to deny
# (ai_docs/bpf-file-open-retry-measurement-20260817.md), so a missing or
# unreadable jq must be reported as a setup fault here rather than surface as a
# verification failure after a 900-second run.
if [[ $verify == 1 ]]; then
  command -v jq >/dev/null 2>&1 \
    || fail "VERIFY=1 requires 'jq' to read the typed verdict; install jq or run without VERIFY=1"
fi

if [[ -z ${INITRAMFS_IMAGE:-} ]]; then
  "$script_dir/qemu-busybox/build-initramfs.sh" "$initramfs_image"
else
  [[ -r $initramfs_image ]] || fail "initramfs is not readable: $initramfs_image"
fi

guest_command=(
  "$script_dir/boot_qemu.sh"
  "$kernel_image"
  "$initramfs_image"
  "$qemu_bin"
)

hermit_args=(--log info --log-file "$info_log" run --strict)
if [[ -n $skid_margin ]]; then
  hermit_args+=(--skid-margin="$skid_margin")
fi
if [[ $verify == 1 ]]; then
  # ⚠️ `--verify-strict` IS LOAD-BEARING FOR THE L2 LABEL PRINTED BELOW.
  # A plain `--verify` stays on the lossy Stripped comparator on every backend
  # -- `RunOpts::verification_strictness` returns `Canonical` only under
  # `--verify-strict`/`--verify-verbose`, and
  # `comparator_choice_does_not_depend_on_the_backend` pins that. AGENTS.md is
  # explicit that default `--verify` "cannot establish L2", so requesting it
  # while printing `level=L2` claimed a tier the run never reached.
  hermit_args+=(--verify --verify-strict --verify-json "$verify_json")
fi
hermit_args+=(--)

printf 'backend=ptrace level=%s log=info relaxations=none\n' \
  "$([[ $verify == 1 ]] && printf L2 || printf L1)"
printf 'pmu_skid_margin=%s\n' "${skid_margin:-auto}"
printf 'hermit=%s\nqemu=%s\nkernel=%s\ninitramfs=%s\nconsole=%s\ninfo=%s\nstderr=%s\n' \
  "$hermit_bin" "$qemu_bin" "$kernel_image" "$initramfs_image" \
  "$console_log" "$info_log" "$stderr_log"
if [[ $verify == 1 ]]; then
  printf 'verify_json=%s\n' "$verify_json"
fi
printf 'kernel_sha256=%s\ninitramfs_sha256=%s\n' \
  "$(sha256sum "$kernel_image" | cut -d' ' -f1)" \
  "$(sha256sum "$initramfs_image" | cut -d' ' -f1)"

: >"$console_log"
: >"$info_log"
: >"$stderr_log"
if [[ $verify == 1 ]]; then
  # Truncate any verdict left by an earlier run: a stale `verify.json` that
  # happened to say `matched` would otherwise certify THIS run.
  : >"$verify_json"
fi
set +e
timeout --signal=TERM --kill-after=10 "${timeout_seconds}s" \
  "$hermit_bin" "${hermit_args[@]}" "${guest_command[@]}" \
  > >(tee "$console_log") \
  2> >(tee "$stderr_log" >&2)
status=$?
set -e

if ((status != 0)); then
  fail "Hermit/QEMU exited with status $status; inspect $console_log, $info_log, and $stderr_log"
fi

# ⚠️ THE WORKLOAD CHECKS BELOW RUN IN BOTH MODES, AND THAT IS DELIBERATE.
# They used to be skipped under VERIFY=1, which meant the stronger mode
# checked strictly LESS about the guest: a BusyBox userspace that never
# reached its marker, or a nested Linux that rejected its clocksource, passed
# unnoticed as long as the two runs agreed. Two runs can agree perfectly on a
# workload that did not do its job -- determinism is not success.
#
# Verify mode CAN see this output: `hermit run --verify` replays the first
# run's captured stdout/stderr to the real descriptors before exiting
# (`run.rs`, `std::io::stdout().write_all(&out1.stdout)`), and the demo tees
# those into $console_log. Only the `--log` DETLOG stream is diverted.
marker=HERMIT-QEMU-BUSYBOX-PASS
grep -Fq "$marker" "$console_log" || fail \
  "guest exited without marker $marker; inspect $console_log"
clock_failures='Unable to calibrate against PIT|Clocksource .* skewed|Marking TSC unstable|No current clocksource'
if grep -Eq "^\[[[:space:]]*[0-9]+\.[0-9]+\].*($clock_failures)" "$console_log"; then
  fail "nested Linux reported a rejected clock failure; inspect $console_log"
fi
printf 'console_sha256=%s\n' \
  "$(sha256sum "$console_log" | cut -d' ' -f1)"

if [[ $verify == 1 ]]; then
  # Read the TYPED verdict, not the banner. ":: Success: deterministic.
  # Determinism verified." is printed by a run whose own --verify-json says
  # `bitwise_parity: false`, so scraping it cannot tell a stripped match from a
  # canonical one -- which is precisely how this demo used to certify L2.
  #
  # These conjuncts mirror `verify_tier_from_json` in
  # tests/backend-parity/run_matrix.py, the repository's enforcing definition of
  # the `bitwise` tier. Keep the two in step. The counts are not redundant: an
  # empty-vs-empty comparison reports "no difference" under the strictest
  # possible spec, so without a positive count a run that produced no DETLOG at
  # all would certify as parity.
  [[ -s $verify_json ]] || fail \
    "VERIFY=1 produced no typed verdict at $verify_json; inspect $stderr_log"
  if ! jq -e '
    .verified == true and
    .verdict == "matched" and
    .bitwise_parity == true and
    .comparison.strictness == "canonical" and
    .comparison.compare_logs == true and
    .comparison.strip_lines == false and
    .comparison.ignore_lines == false and
    .comparison.skip_commit == false and
    .comparison.skip_detlog == false and
    (.compared_log_messages.left | type) == "number" and
    (.compared_log_messages.right | type) == "number" and
    .compared_log_messages.left > 0 and
    .compared_log_messages.right > 0 and
    .compared_log_messages.left == .compared_log_messages.right
  ' "$verify_json" >/dev/null; then
    fail "Hermit did not emit a matched non-vacuous canonical verdict; inspect $verify_json and $stderr_log"
  fi
  printf 'compared_info_messages=%s\n' \
    "$(jq -r '.compared_log_messages.left' "$verify_json")"
fi

printf 'PASS: BusyBox userspace completed under Hermit/QEMU (%s, ptrace backend)\n' \
  "$([[ $verify == 1 ]] && printf L2 || printf L1)"
