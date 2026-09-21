/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>

/*
 * PR_SET_PDEATHSIG / PR_GET_PDEATHSIG configure and query the calling thread's
 * parent-death signal -- the signal the kernel would deliver if the parent
 * exits. This contract exercises only the attribute STATE machine: it sets,
 * changes, and clears the parent-death signal and reads each value back. It
 * never arranges for the parent to die, so no signal is delivered and no
 * scheduling or timing channel is involved -- exactly like the sigprocmask and
 * sigaction state contracts. Setting a value and reading back the value just set
 * is a pure per-thread register with no host-derived state, so it is
 * deterministic across repeated runs and backends.
 *
 * ptrace and DBT drive the full state machine; KVM's ElfExecutor does not
 * implement the PR_*_PDEATHSIG requests and refuses them with ENOSYS (recorded
 * as a KVM gap in matrix.tsv), so this row runs on ptrace and DBT.
 *
 * EMISSION CONTRACT: the fixture prints the signal number read back after each
 * set, not just a check count. The values are guest-chosen (SIGUSR1, then
 * SIGUSR2, then cleared), so they are host-independent, and printing them makes
 * a wrong read-back visible in the byte stream instead of only in a lower total.
 */

int main(void) {
  enum { EXPECTED_CHECKS = 6 };
  int ok = 0;

  /* Set the parent-death signal to SIGUSR1 and read it back. */
  if (prctl(PR_SET_PDEATHSIG, SIGUSR1, 0, 0, 0) == 0) {
    ok++;
  } else {
    fprintf(stderr, "PR_SET_PDEATHSIG(SIGUSR1) errno %d\n", errno);
    return 1;
  }
  int got1 = -1;
  if (prctl(PR_GET_PDEATHSIG, &got1, 0, 0, 0) == 0 && got1 == SIGUSR1) {
    ok++;
  } else {
    fprintf(stderr, "PR_GET_PDEATHSIG got %d errno %d (want %d)\n", got1, errno, SIGUSR1);
    return 1;
  }

  /* Change it to SIGUSR2 and confirm the new value. */
  if (prctl(PR_SET_PDEATHSIG, SIGUSR2, 0, 0, 0) == 0) {
    ok++;
  } else {
    fprintf(stderr, "PR_SET_PDEATHSIG(SIGUSR2) errno %d\n", errno);
    return 1;
  }
  int got2 = -1;
  if (prctl(PR_GET_PDEATHSIG, &got2, 0, 0, 0) == 0 && got2 == SIGUSR2) {
    ok++;
  } else {
    fprintf(stderr, "PR_GET_PDEATHSIG got %d errno %d (want %d)\n", got2, errno, SIGUSR2);
    return 1;
  }

  /* Clear it (signal 0) and confirm the cleared state. */
  if (prctl(PR_SET_PDEATHSIG, 0, 0, 0, 0) == 0) {
    ok++;
  } else {
    fprintf(stderr, "PR_SET_PDEATHSIG(0) errno %d\n", errno);
    return 1;
  }
  int got3 = -1;
  if (prctl(PR_GET_PDEATHSIG, &got3, 0, 0, 0) == 0 && got3 == 0) {
    ok++;
  } else {
    fprintf(stderr, "PR_GET_PDEATHSIG got %d errno %d (want 0)\n", got3, errno);
    return 1;
  }

  printf("pdeathsig ok=%d set_usr1_readback=%d set_usr2_readback=%d "
         "cleared_readback=%d\n",
         ok, got1, got2, got3);
  return ok == 6 ? 0 : 1;
}
