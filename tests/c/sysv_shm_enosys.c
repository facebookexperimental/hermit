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

#ifndef SYS_shmget
#define SYS_shmget 29
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_shmget, 0, 0, 0);
  if (result == -1 && errno == ENOSYS) {
    puts("System V shared memory deterministically unavailable");
    return 0;
  }

  fprintf(stderr, "shmget: expected ENOSYS, got result=%ld errno=%d\n",
          result, errno);
  return 1;
}
