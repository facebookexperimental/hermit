/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Backend-parity contract: prctl(2) task-control round-trips are deterministic
 * and independent of host process state.
 *
 * Every checked operation reads back a value the guest itself just set (or a
 * fixed post-exec default), so the observable result depends only on the
 * program, not on the host task's inherited name, dumpable flag, keepcaps bit,
 * or parent-death signal. That makes the contract byte-identical across repeated
 * runs and across the ptrace, DBT, and KVM backends. It uses no threads, no
 * blocking I/O, and no signal delivery, so it is safe under the DBT
 * no-preemption scheduler.
 *
 * EMISSION CONTRACT: the fixture prints every value it read back, not the bare
 * success token "prctl-identity-ok" it used to print. Every emitted value is one
 * the guest itself installed (or the fixed post-exec pdeathsig default), so the
 * line stays host-independent while making a wrong read-back visible in the byte
 * stream. The old token carried no observation at all -- strictly less
 * informative than a tally, which at least reports how many checks passed.
 */

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <unistd.h>

static int fail(const char *what) {
  fprintf(stderr, "prctl-identity: %s failed: %s\n", what, strerror(errno));
  return 1;
}

int main(void) {
  /* PR_SET_NAME / PR_GET_NAME: the thread name is capped at 16 bytes including
   * the terminator and must read back exactly what we set. */
  const char *wanted = "hermit-probe";
  if (prctl(PR_SET_NAME, wanted, 0, 0, 0) != 0)
    return fail("PR_SET_NAME");
  char name[16] = {0};
  if (prctl(PR_GET_NAME, name, 0, 0, 0) != 0)
    return fail("PR_GET_NAME");
  if (strcmp(name, wanted) != 0) {
    fprintf(stderr, "prctl-identity: name mismatch: %s\n", name);
    return 1;
  }

  /* PR_SET_DUMPABLE / PR_GET_DUMPABLE: a set value reads straight back. */
  if (prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) != 0)
    return fail("PR_SET_DUMPABLE 0");
  const int dumpable_after_clear = prctl(PR_GET_DUMPABLE, 0, 0, 0, 0);
  if (dumpable_after_clear != 0) {
    fprintf(stderr, "prctl-identity: dumpable not 0\n");
    return 1;
  }
  if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0)
    return fail("PR_SET_DUMPABLE 1");
  const int dumpable_after_set = prctl(PR_GET_DUMPABLE, 0, 0, 0, 0);
  if (dumpable_after_set != 1) {
    fprintf(stderr, "prctl-identity: dumpable not 1\n");
    return 1;
  }

  /* PR_SET_KEEPCAPS / PR_GET_KEEPCAPS: boolean round-trip. */
  if (prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0) != 0)
    return fail("PR_SET_KEEPCAPS 1");
  const int keepcaps_after_set = prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0);
  if (keepcaps_after_set != 1) {
    fprintf(stderr, "prctl-identity: keepcaps not 1\n");
    return 1;
  }
  if (prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0) != 0)
    return fail("PR_SET_KEEPCAPS 0");
  const int keepcaps_after_clear = prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0);
  if (keepcaps_after_clear != 0) {
    fprintf(stderr, "prctl-identity: keepcaps not 0\n");
    return 1;
  }

  /* PR_GET_PDEATHSIG: a fresh exec has no parent-death signal installed. */
  int pdeath = -1;
  if (prctl(PR_GET_PDEATHSIG, &pdeath, 0, 0, 0) != 0)
    return fail("PR_GET_PDEATHSIG");
  if (pdeath != 0) {
    fprintf(stderr, "prctl-identity: pdeathsig not 0: %d\n", pdeath);
    return 1;
  }

  /* Emit every read-back value rather than a success token. */
  printf("prctl-identity name=%s dumpable_after_clear=%d dumpable_after_set=%d "
         "keepcaps_after_set=%d keepcaps_after_clear=%d pdeathsig_initial=%d\n",
         name, dumpable_after_clear, dumpable_after_set, keepcaps_after_set,
         keepcaps_after_clear, pdeath);
  return 0;
}
