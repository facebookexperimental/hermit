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

#ifndef SYS_kcmp
#define SYS_kcmp 312
#endif

#define KCMP_FILE 0

int main(void) {
  errno = 0;
  long result = syscall(SYS_kcmp, getpid(), 1, KCMP_FILE, 0, 0);
  if (result == -1 && errno == EPERM) {
    puts("kcmp deterministically refused");
    return 0;
  }

  fprintf(stderr, "kcmp: expected EPERM, got result=%ld errno=%d\n", result,
          errno);
  return 1;
}
