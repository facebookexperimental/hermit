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
#include <stdlib.h>
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
 * never asserts the glibc-registered initial head's address (the runtime
 * chooses it), so the row is deterministic without depending on cross-run
 * address stability, and it prints only a check count. It does assert that
 * head's shape: present, with the kernel's head length. That check carried the
 * former c-programs/get-robust-list-self row, merged here by the 2026-10-17
 * test audit; tests/c/get_robust_list_self.c stays for
 * hermit-cli/tests/robust_list_queries.rs.
 */

struct robust_head {
  void* next;
  long futex_offset;
  void* list_op_pending;
};

int main(void) {
  enum { EXPECTED_CHECKS = 6 };
  int ok = 0;

  /* The initial query (glibc has already registered a head) must succeed and
   * report a head of the kernel's length; its address is not asserted. */
  void* head0 = NULL;
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
  if (head0 != NULL && len0 == sizeof(struct robust_head)) {
    ok++;
  } else {
    fprintf(
        stderr,
        "initial robust-list head %s, length %zu (expected %zu)\n",
        head0 == NULL ? "missing" : "present",
        len0,
        sizeof(struct robust_head));
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

  void* head1 = NULL;
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
    fprintf(
        stderr,
        "round-trip head mismatch: got %p want %p\n",
        head1,
        (void*)&mine);
    return 1;
  }

  if (len1 == sizeof(mine)) {
    ok++;
  } else {
    fprintf(
        stderr,
        "round-trip len mismatch: got %zu want %zu\n",
        len1,
        sizeof(mine));
    return 1;
  }

  printf("robustlist ok=%d\n", ok);
  return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
