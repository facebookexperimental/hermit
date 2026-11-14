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
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

/*
 * Probe for the two in-range gaps in Hermit's pinned x86-64 syscall table:
 * 335 (uretprobe) and 336 (uprobe). On kernels that wire them, entry code
 * handles them before seccomp/ptrace, so the ptrace backend cannot
 * intercept them and the guest keeps the native outcome.
 * hermit-cli/tests/unknown_syscall_refusal.rs runs this guest natively and
 * under Hermit and pins the outcomes to be equal.
 *
 * Usage: uprobe_gap_probe 335|336
 * Prints a flushed marker before the call (335 outside a uretprobe
 * trampoline kills the caller with SIGILL, so it may never print again),
 * then the call's result when it returns.
 */
int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s 335|336\n", argv[0]);
    return 2;
  }
  char *end = NULL;
  long nr = strtol(argv[1], &end, 10);
  if (end == argv[1] || *end != '\0' || (nr != 335 && nr != 336)) {
    fprintf(stderr, "usage: %s 335|336\n", argv[0]);
    return 2;
  }
  printf("uprobe-gap-before nr %ld\n", nr);
  fflush(stdout);
  errno = 0;
  long result = syscall(nr, 0, 0, 0);
  int seen = errno;
  printf("nr %ld ret %ld errno %d (%s)\n", nr, result, seen, strerror(seen));
  fflush(stdout);
  return 0;
}
