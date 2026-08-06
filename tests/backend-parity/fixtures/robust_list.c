/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

/*
 * set_robust_list/get_robust_list register the calling thread's robust-futex
 * list head, which the kernel walks to release futexes when a thread dies.
 * Registration is a pure per-thread pointer store: it involves no futex wait,
 * no scheduling decision, and no host-derived state. This contract asserts the
 * process-local round trip only -- after the fixture registers its own head,
 * get_robust_list must hand back the exact pointer and length it just set. It
 * never asserts the glibc-registered initial head (an address the runtime
 * chooses), so the row is deterministic without depending on cross-run address
 * stability, and it prints only a check count.
 */

struct robust_head {
  void *next;
  long futex_offset;
  void *list_op_pending;
};

int main(void) {
  int ok = 0;

  /* The initial query (glibc has already registered a head) must succeed; its
   * value is not asserted. */
  void *head0 = NULL;
  size_t len0 = 0;
  errno = 0;
  long result = syscall(SYS_get_robust_list, 0, &head0, &len0);
  if (result == 0) {
    ok++;
  } else {
    fprintf(
        stderr,
        "initial get_robust_list returned %ld errno %d (%s)\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }

  /* Register our own head; len must equal sizeof(struct robust_list_head) or
   * the kernel rejects it with EINVAL. */
  struct robust_head mine;
  memset(&mine, 0, sizeof(mine));
  errno = 0;
  result = syscall(SYS_set_robust_list, &mine, sizeof(mine));
  if (result == 0) {
    ok++;
  } else {
    fprintf(
        stderr,
        "set_robust_list returned %ld errno %d (%s)\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }

  void *head1 = NULL;
  size_t len1 = 0;
  errno = 0;
  result = syscall(SYS_get_robust_list, 0, &head1, &len1);
  if (result == 0) {
    ok++;
  } else {
    fprintf(
        stderr,
        "re-get get_robust_list returned %ld errno %d (%s)\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }

  if (head1 == &mine) {
    ok++;
  } else {
    fprintf(stderr, "round-trip head mismatch: got %p want %p\n", head1, (void *)&mine);
    return 1;
  }

  if (len1 == sizeof(mine)) {
    ok++;
  } else {
    fprintf(stderr, "round-trip len mismatch: got %zu want %zu\n", len1, sizeof(mine));
    return 1;
  }

  printf("robustlist ok=%d\n", ok);
  return 0;
}
