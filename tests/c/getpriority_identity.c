/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * getpriority_identity — backend-parity contract for the getpriority(2) /
 * setpriority(2) determinization.
 *
 * The nice value is inert under Detcore's virtualized single-CPU scheduler, so
 * Detcore reports a constant priority independent of the host: raw
 * getpriority(2) returns the fixed kernel value 20 (nice 0) for any valid
 * `which`, and setpriority(2) is accepted as an inert no-op (returns 0) that
 * never actually changes the reported priority. That constant is what makes the
 * value bitwise-identical across --verify repeat runs and under record/replay,
 * and it must match across backends: DBT and KVM have to mirror the golden
 * ptrace reference exactly.
 *
 * This fixture uses the RAW syscalls, not glibc's getpriority()/setpriority()
 * wrappers. glibc's getpriority() wrapper translates the kernel's 1..40 return
 * into a signed nice value (nice = 20 - raw), so only the raw syscall exposes
 * exactly the constant Detcore's handler returns. It (1) reads the raw priority
 * repeatedly and asserts the fixed 20, (2) issues setpriority to a different
 * nice level and asserts the inert 0, (3) re-reads and asserts the priority is
 * STILL 20 — proving the write was virtualized away rather than applied — and
 * (4) checks that an invalid `which` still faults with EINVAL, preserving the
 * Linux boundary. On a real host step (3) would observe the changed nice, so a
 * native run diverges, proving this pins a genuine determinization rather than a
 * tautology.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Detcore's fixed raw getpriority return: kernel 20 == nice 0. */
#define VIRTUAL_RAW_PRIORITY 20
/* A nice level distinct from the virtualized default, used to prove inertness. */
#define ATTEMPTED_NICE 10
/* PRIO_PROCESS is 0 on Linux; spell it out to avoid wrapper header reliance. */
#define WHICH_PROCESS 0
/* An out-of-range `which` that Linux (and Detcore) reject with EINVAL. */
#define WHICH_INVALID 999

static int expect_raw_priority(int iter) {
  long ret = syscall(SYS_getpriority, WHICH_PROCESS, 0);
  if (ret != VIRTUAL_RAW_PRIORITY) {
    fprintf(stderr, "iter %d: getpriority returned %ld, expected %d (nice 0)\n",
            iter, ret, VIRTUAL_RAW_PRIORITY);
    return 1;
  }
  return 0;
}

int main(void) {
  /* Repeat to prove the determinized answer is stable, not incidental. */
  for (int i = 0; i < 4; i++) {
    if (expect_raw_priority(i)) {
      return 1;
    }
  }

  /* setpriority must be accepted as an inert no-op returning 0. */
  long set_ret = syscall(SYS_setpriority, WHICH_PROCESS, 0, ATTEMPTED_NICE);
  if (set_ret != 0) {
    fprintf(stderr, "setpriority returned %ld, expected 0 (inert no-op)\n",
            set_ret);
    return 1;
  }

  /* The attempted nice change must not be reflected: still the constant 20. */
  if (expect_raw_priority(4)) {
    fprintf(stderr, "setpriority leaked into getpriority (nice not virtualized)\n");
    return 1;
  }

  /* An invalid `which` preserves the Linux EINVAL boundary. */
  long bad = syscall(SYS_getpriority, WHICH_INVALID, 0);
  if (bad != -1 || errno != EINVAL) {
    fprintf(stderr,
            "getpriority(invalid which) returned %ld errno %d, expected -1 EINVAL\n",
            bad, errno);
    return 1;
  }

  puts("getpriority-identity-ok");
  return 0;
}
