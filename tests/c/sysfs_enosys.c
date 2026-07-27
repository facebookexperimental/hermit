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

#ifndef SYS_sysfs
#define SYS_sysfs 139
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_sysfs, 1, "ext4");
  if (result != -1 || errno != ENOSYS) {
    fprintf(
        stderr,
        "sysfs returned %ld with errno %d (%s), expected ENOSYS\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }
  puts("sysfs deterministically unavailable");
  return 0;
}
