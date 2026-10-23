/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef HERMIT_TESTS_C_PROBE_FAMILY_H
#define HERMIT_TESTS_C_PROBE_FAMILY_H

/*
 * Run each member of a probe family in its own child process, in order, and
 * fail if any member fails. A family replaces one manifest test per member
 * (the 2026-10-17 test audit), and one Hermit launch covers them all. Each
 * member starts in a fresh process with no memory or descriptors of an
 * earlier member, but the members share that one Hermit run: Detcore state
 * that outlives a process (virtual time, pid numbering, the port allocator)
 * carries from one member to the next. Merge only a family whose members'
 * checks do not depend on that state. A member that fails, dies by a signal,
 * or cannot be started makes the family exit 1 after the remaining members
 * have run, so one failure never hides another.
 */

#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

typedef int (*probe_member)(void);

static int run_probe_family(const probe_member* members, int count) {
  int failed = 0;
  for (int index = 0; index < count; ++index) {
    fflush(stdout);
    fflush(stderr);
    const pid_t child = fork();
    if (child < 0) {
      perror("fork");
      failed = 1;
      continue;
    }
    if (child == 0) {
      const int status = members[index]();
      fflush(stdout);
      fflush(stderr);
      _exit(status == 0 ? 0 : 1);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      perror("waitpid");
      failed = 1;
      continue;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
      fprintf(stderr, "probe family member %d failed (status %#x)\n", index, status);
      failed = 1;
    }
  }
  return failed;
}

#endif
