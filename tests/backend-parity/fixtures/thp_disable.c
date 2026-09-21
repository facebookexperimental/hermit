/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>

/*
 * PR_SET_THP_DISABLE / PR_GET_THP_DISABLE toggle and query the calling
 * process's "transparent hugepage disabled" flag. This contract exercises only
 * the attribute STATE machine: it sets the flag, reads it back, clears it, and
 * reads it back again. It is a pure per-process boolean register with no
 * host-derived state -- setting a value and reading back the value just set is
 * deterministic across repeated runs and backends, exactly like the
 * PR_SET_DUMPABLE / PR_GET_DUMPABLE round-trip in the process-identity contract.
 * It touches no memory mapping, allocation, signal, scheduling, or timing
 * channel; the flag is a policy hint the kernel records but the fixture never
 * relies on any hugepage-backing side effect.
 *
 * ptrace and DBT drive the full round-trip; KVM's ElfExecutor does not implement
 * the PR_*_THP_DISABLE requests and refuses them with ENOSYS (recorded as a KVM
 * gap in matrix.tsv), so this row runs on ptrace and DBT. The fixture prints
 * only a check count.
 */

#ifndef PR_SET_THP_DISABLE
#define PR_SET_THP_DISABLE 41
#endif
#ifndef PR_GET_THP_DISABLE
#define PR_GET_THP_DISABLE 42
#endif

/*
 * THE READ-BACK FLAG VALUES ARE EMITTED. This is a set/read-back round trip, so
 * the flag the guest reads back after each set IS the observation, and
 * "thp ok=4" hid it. A backend that ignored the set and one that returned a
 * garbage flag value were indistinguishable, and the values went only to stderr,
 * which the cell observation excludes.
 *
 * The read-backs are guest-determined -- the guest set the flag to those exact
 * values one call earlier -- so emitting them adds no host state.
 */
int main(void) {
  enum { EXPECTED_CHECKS = 4 };

  /* Disable transparent hugepages for this process and read the flag back. */
  int set_on = prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0) == 0;
  int get_after_set = prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0);

  /* Re-enable transparent hugepages and confirm the cleared state. */
  int set_off = prctl(PR_SET_THP_DISABLE, 0, 0, 0, 0) == 0;
  int get_after_clear = prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0);

  int ok = set_on + (get_after_set == 1) + set_off + (get_after_clear == 0);
  printf(
      "thp ok=%d set_on=%d get_after_set=%d set_off=%d get_after_clear=%d\n",
      ok,
      set_on,
      get_after_set,
      set_off,
      get_after_clear);
  return ok == EXPECTED_CHECKS ? 0 : 1;
}
