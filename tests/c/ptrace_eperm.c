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
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_ptrace
#define SYS_ptrace 101
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_ptrace, PTRACE_ATTACH, 1, NULL, NULL);
  if (result == -1 && errno == EPERM) {
    puts("ptrace deterministically refused");
    return 0;
  }

  fprintf(stderr, "ptrace: expected EPERM, got result=%ld errno=%d\n", result,
          errno);
  return 1;
}
