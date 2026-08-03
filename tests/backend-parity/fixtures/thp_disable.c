/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
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
 * ptrace and DBI drive the full round-trip; KVM's ElfExecutor does not implement
 * the PR_*_THP_DISABLE requests and refuses them with ENOSYS (recorded as a KVM
 * gap in matrix.tsv), so this row runs on ptrace and DBI. The fixture prints
 * only a check count.
 */

#ifndef PR_SET_THP_DISABLE
#define PR_SET_THP_DISABLE 41
#endif
#ifndef PR_GET_THP_DISABLE
#define PR_GET_THP_DISABLE 42
#endif

int main(void) {
  int ok = 0;

  /* Disable transparent hugepages for this process and read the flag back. */
  if (prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0) == 0) {
    ok++;
  } else {
    fprintf(stderr, "PR_SET_THP_DISABLE(1) errno %d\n", errno);
    return 1;
  }
  if (prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0) == 1) {
    ok++;
  } else {
    fprintf(stderr, "PR_GET_THP_DISABLE after set errno %d\n", errno);
    return 1;
  }

  /* Re-enable transparent hugepages and confirm the cleared state. */
  if (prctl(PR_SET_THP_DISABLE, 0, 0, 0, 0) == 0) {
    ok++;
  } else {
    fprintf(stderr, "PR_SET_THP_DISABLE(0) errno %d\n", errno);
    return 1;
  }
  if (prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0) == 0) {
    ok++;
  } else {
    fprintf(stderr, "PR_GET_THP_DISABLE after clear errno %d\n", errno);
    return 1;
  }

  printf("thp ok=%d\n", ok);
  return 0;
}
