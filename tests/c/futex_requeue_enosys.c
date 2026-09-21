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

int main(void) {
  errno = 0;
  long result = syscall(SYS_futex_requeue, NULL, 0, 0, 0);
  if (result != -1 || errno != ENOSYS) {
    fprintf(stderr,
            "futex_requeue returned %ld with errno %d (%s), expected ENOSYS\n",
            result, errno, strerror(errno));
    return 1;
  }
  puts("futex-requeue-enosys-ok");
  return 0;
}
