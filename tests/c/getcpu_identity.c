/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * getcpu_identity — backend-parity contract for the getcpu(2) determinization.
 *
 * Detcore virtualizes a single logical CPU on a single virtual NUMA node, so
 * the raw SYS_getcpu syscall must always report CPU 0 / node 0 and succeed,
 * regardless of which host CPU actually ran the guest. That constant answer is
 * what makes the value bitwise-identical across --verify repeat runs and under
 * record/replay, and it must be identical across backends: the DBI backend has
 * to match the golden ptrace reference exactly.
 *
 * This fixture exercises the raw syscall (not glibc's vDSO-accelerated
 * sched_getcpu, which does not route through syscall interception) across the
 * optional-output-pointer combinations that detcore's handler special-cases —
 * both pointers set, cpu-only, node-only — and repeats the query to prove the
 * answer does not drift. Any nonzero CPU/node or a differing value between
 * calls means the container leaked host topology or the backend diverged.
 */

#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Poison sentinel: if detcore fails to write the output, the check catches it. */
#define SENTINEL 0x7fu

static int query(unsigned *cpu_out, unsigned *node_out) {
  if (cpu_out) {
    *cpu_out = SENTINEL;
  }
  if (node_out) {
    *node_out = SENTINEL;
  }
  /* Third argument (tcache) has been ignored by the kernel since Linux 2.6.24. */
  return (int)syscall(SYS_getcpu, cpu_out, node_out, NULL);
}

int main(void) {
  unsigned cpu = SENTINEL;
  unsigned node = SENTINEL;

  /* Repeat to prove the determinized answer is stable, not incidental. */
  for (int i = 0; i < 4; i++) {
    /* Both output pointers set. */
    cpu = SENTINEL;
    node = SENTINEL;
    if (query(&cpu, &node) != 0) {
      fprintf(stderr, "iter %d: getcpu(cpu,node) did not return 0\n", i);
      return 1;
    }
    if (cpu != 0 || node != 0) {
      fprintf(stderr, "iter %d: getcpu(cpu,node) reported cpu=%u node=%u, expected 0/0\n",
              i, cpu, node);
      return 1;
    }

    /* cpu only (node pointer NULL): handler must still write cpu=0. */
    cpu = SENTINEL;
    if (query(&cpu, NULL) != 0 || cpu != 0) {
      fprintf(stderr, "iter %d: getcpu(cpu,NULL) reported cpu=%u, expected 0\n", i, cpu);
      return 1;
    }

    /* node only (cpu pointer NULL): handler must still write node=0. */
    node = SENTINEL;
    if (query(NULL, &node) != 0 || node != 0) {
      fprintf(stderr, "iter %d: getcpu(NULL,node) reported node=%u, expected 0\n", i, node);
      return 1;
    }
  }

  puts("getcpu-identity-ok");
  return 0;
}
