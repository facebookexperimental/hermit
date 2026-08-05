/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * sched_getaffinity_identity — backend-parity contract for the
 * sched_getaffinity(2) determinization.
 *
 * Detcore virtualizes a single logical CPU, so it suppresses the host affinity
 * mask and reports a fixed cpuset containing only CPU 0, returning a fixed
 * 16-byte (VIRTUAL_CPUSET_BYTES) cpumask size regardless of how many CPUs the
 * host actually has. That constant answer is what makes the value
 * bitwise-identical across --verify repeat runs and under record/replay, and it
 * must be identical across backends: the DBI and KVM backends have to match the
 * golden ptrace reference exactly.
 *
 * This fixture exercises the raw syscall (not glibc's sched_getaffinity
 * wrapper, which post-processes the kernel result) so it observes precisely
 * what detcore's handler returns. It poisons the mask buffer with a sentinel,
 * issues the syscall repeatedly, and asserts on every call that the return
 * value is the virtualized 16, that CPU 0 is the only bit set, and that no
 * other byte of the mask is nonzero. On a real host the syscall returns the
 * kernel's native cpumask size and a mask reflecting the host's many CPUs, so a
 * native run diverges — proving this fixture pins a genuine determinization
 * rather than a tautology.
 */

#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Detcore's VIRTUAL_CPUSET_BYTES: the fixed cpumask size it reports. */
#define VIRTUAL_CPUSET_BYTES 16
/* Poison sentinel: if detcore fails to overwrite a byte, the check catches it. */
#define SENTINEL 0x7fu

int main(void) {
  unsigned char mask[VIRTUAL_CPUSET_BYTES];

  /* Repeat to prove the determinized answer is stable, not incidental. */
  for (int i = 0; i < 4; i++) {
    for (int b = 0; b < VIRTUAL_CPUSET_BYTES; b++) {
      mask[b] = SENTINEL;
    }

    long ret = syscall(SYS_sched_getaffinity, 0, (size_t)VIRTUAL_CPUSET_BYTES, mask);

    if (ret != VIRTUAL_CPUSET_BYTES) {
      fprintf(stderr,
              "iter %d: sched_getaffinity returned %ld, expected %d\n",
              i, ret, VIRTUAL_CPUSET_BYTES);
      return 1;
    }

    /* CPU 0 must be the only online CPU in the virtualized mask. */
    if (mask[0] != 1) {
      fprintf(stderr, "iter %d: mask byte 0 = 0x%02x, expected 0x01 (CPU 0 only)\n",
              i, mask[0]);
      return 1;
    }
    for (int b = 1; b < VIRTUAL_CPUSET_BYTES; b++) {
      if (mask[b] != 0) {
        fprintf(stderr,
                "iter %d: mask byte %d = 0x%02x, expected 0x00 (no CPU beyond 0)\n",
                i, b, mask[b]);
        return 1;
      }
    }
  }

  puts("sched-getaffinity-identity-ok");
  return 0;
}
