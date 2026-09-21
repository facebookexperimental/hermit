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
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_process_mrelease
#define SYS_process_mrelease 448
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_process_mrelease, -1, 0U);
  if (result != -1 || errno != ENOSYS) {
    fprintf(
        stderr,
        "process_mrelease returned %ld with errno %d (%s), expected ENOSYS\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }
  puts("process_mrelease deterministically unavailable");
  return 0;
}
