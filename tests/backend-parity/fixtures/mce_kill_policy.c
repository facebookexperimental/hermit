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
 * PR_MCE_KILL / PR_MCE_KILL_GET set and query the calling process's
 * machine-check (memory-error) kill policy: early kill, late kill, or the
 * system default. The policy governs how the kernel reacts to an uncorrectable
 * hardware memory error on a page the process maps -- a hardware-fault channel
 * the deterministic container does not model.
 *
 * All three Hermit backends therefore refuse both the set and the query
 * deterministically with ENOSYS, exactly as they refuse seccomp introspection,
 * SysV IPC creation, and PR_SET_CHILD_SUBREAPER. Outside Hermit the same calls
 * succeed (the native kernel records the policy), so this is a determinization
 * choice rather than a host limitation. The refusal is a fixed errno with no
 * host-derived state, so it is identical across repeated runs and all backends.
 *
 * The fixture never arranges a machine-check event, so it touches no memory
 * fault, signal, scheduling, or timing channel; it asserts only the two ENOSYS
 * refusals and prints a check count.
 */

#ifndef PR_MCE_KILL
#define PR_MCE_KILL 33
#endif
#ifndef PR_MCE_KILL_GET
#define PR_MCE_KILL_GET 34
#endif
#ifndef PR_MCE_KILL_SET
#define PR_MCE_KILL_SET 1
#endif
#ifndef PR_MCE_KILL_EARLY
#define PR_MCE_KILL_EARLY 1
#endif

int main(void) {
  int ok = 0;

  /* Setting the machine-check kill policy is refused with a fixed ENOSYS. */
  errno = 0;
  if (prctl(PR_MCE_KILL, PR_MCE_KILL_SET, PR_MCE_KILL_EARLY, 0, 0) == -1 &&
      errno == ENOSYS) {
    ok++;
  } else {
    fprintf(stderr, "PR_MCE_KILL_SET not refused: errno %d\n", errno);
    return 1;
  }

  /* Querying the machine-check kill policy is refused with a fixed ENOSYS. */
  errno = 0;
  if (prctl(PR_MCE_KILL_GET, 0, 0, 0, 0) == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(stderr, "PR_MCE_KILL_GET not refused: errno %d\n", errno);
    return 1;
  }

  printf("mcekill ok=%d\n", ok);
  return 0;
}
