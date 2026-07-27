/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <linux/mman.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_cachestat
#define SYS_cachestat 451
#endif

int main(void) {
  struct cachestat_range range;
  struct cachestat result_buf;
  memset(&range, 0, sizeof(range));
  memset(&result_buf, 0, sizeof(result_buf));

  errno = 0;
  long result = syscall(SYS_cachestat, -1, &range, &result_buf, 0U);
  if (result != -1 || errno != ENOSYS) {
    fprintf(stderr,
            "cachestat returned %ld with errno %d (%s), expected ENOSYS\n",
            result, errno, strerror(errno));
    return 1;
  }
  puts("cachestat deterministically unavailable");
  return 0;
}
