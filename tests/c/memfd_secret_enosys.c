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

#ifndef SYS_memfd_secret
#define SYS_memfd_secret 447
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_memfd_secret, 0U);
  if (result != -1 || errno != ENOSYS) {
    fprintf(
        stderr,
        "memfd_secret returned %ld with errno %d (%s), expected ENOSYS\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }
  puts("memfd_secret deterministically unavailable");
  return 0;
}
