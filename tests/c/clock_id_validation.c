/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Prints whether clock_gettime and clock_getres accept each clock id, as the
// result and errno only, never a time, so the output is the same natively and
// under Hermit on every host. The ids are the fixed clocks Linux knows (0 to
// 7 and CLOCK_TAI, 11), the retired CLOCK_SGI_CYCLE (10), ids past the end
// (12, 16, 1000), and this process's and this thread's CPU clocks (-6, -2).
// The alarm clocks (8, 9) are left out: Linux accepts them only on a host
// with a real-time clock.

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

int main(void) {
  static const int ids[] = {0, 1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 16, 1000, -6, -2};
  for (size_t i = 0; i < sizeof(ids) / sizeof(ids[0]); i++) {
    struct timespec value;
    errno = 0;
    long gettime = syscall(SYS_clock_gettime, ids[i], &value);
    int gettime_errno = errno;
    errno = 0;
    long getres = syscall(SYS_clock_getres, ids[i], &value);
    int getres_errno = errno;
    printf(
        "clock %d: gettime %ld %s, getres %ld %s\n",
        ids[i],
        gettime,
        gettime == 0 ? "ok" : strerrorname_np(gettime_errno),
        getres,
        getres == 0 ? "ok" : strerrorname_np(getres_errno));
  }
  return 0;
}
