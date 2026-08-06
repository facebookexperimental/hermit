/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

/*
 * seccomp introspection refusal.
 *
 * Outside Hermit both seccomp query paths succeed: PR_GET_SECCOMP returns the
 * calling thread's seccomp mode (0 when unconfined) and seccomp(2) with
 * SECCOMP_GET_ACTION_AVAIL confirms a supported filter action. Under Hermit all
 * three backends deterministically refuse both paths with a fixed errno --
 * ENOSYS for the prctl query and EOPNOTSUPP for the seccomp(2) query -- so a
 * guest cannot observe or install a kernel syscall filter that would perturb
 * Hermit's own syscall interception. Like the io_uring and listmount refusal
 * contracts, the native call may succeed while every Hermit backend returns the
 * same deterministic error; the fixture asserts only the refusal, never a
 * host-derived seccomp mode value, so it stays byte-identical across repeated
 * runs and backends. It exercises no signal delivery, scheduling, or timing
 * channel.
 */

#ifndef SECCOMP_GET_ACTION_AVAIL
#define SECCOMP_GET_ACTION_AVAIL 2
#endif

int main(void) {
  int ok = 0;

  /* PR_GET_SECCOMP: query the calling thread's seccomp mode. */
  errno = 0;
  if (prctl(PR_GET_SECCOMP, 0, 0, 0, 0) == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(stderr, "PR_GET_SECCOMP not refused with ENOSYS: errno %d\n", errno);
    return 1;
  }

  /* seccomp(2) SECCOMP_GET_ACTION_AVAIL: query filter-action support. */
  unsigned int action = 0x7fff0000u; /* SECCOMP_RET_ALLOW */
  errno = 0;
  if (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, &action) == -1 &&
      errno == EOPNOTSUPP) {
    ok++;
  } else {
    fprintf(stderr, "seccomp(2) not refused with EOPNOTSUPP: errno %d\n", errno);
    return 1;
  }

  printf("seccomp ok=%d\n", ok);
  return 0;
}
