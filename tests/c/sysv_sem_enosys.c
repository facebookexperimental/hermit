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
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_semget
#define SYS_semget 64
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_semget, 0, 0, 0);
  if (result == -1 && errno == ENOSYS) {
    puts("System V semaphores deterministically unavailable");
    return 0;
  }

  fprintf(stderr, "semget: expected ENOSYS, got result=%ld errno=%d\n",
          result, errno);
  return 1;
}
