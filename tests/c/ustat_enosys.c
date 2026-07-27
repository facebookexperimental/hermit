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
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_ustat
#define SYS_ustat 136
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_ustat, 0, NULL);
  if (result == -1 && errno == ENOSYS) {
    puts("ustat deterministically unavailable");
    return 0;
  }

  fprintf(stderr, "ustat: expected ENOSYS, got result=%ld errno=%d\n", result,
          errno);
  return 1;
}
