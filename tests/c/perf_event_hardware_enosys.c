/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <linux/perf_event.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  struct perf_event_attr attr = {
      .type = PERF_TYPE_HARDWARE,
      .size = sizeof(attr),
      .config = PERF_COUNT_HW_INSTRUCTIONS,
      .disabled = 1,
      .exclude_kernel = 1,
      .exclude_hv = 1,
  };

  errno = 0;
  long result = syscall(SYS_perf_event_open, &attr, 0, -1, -1, 0);
  if (result != -1 || errno != ENOSYS) {
    fprintf(stderr,
            "hardware perf_event_open returned %ld with errno %d (%s), "
            "expected ENOSYS\n",
            result, errno, strerror(errno));
    if (result >= 0) {
      close((int)result);
    }
    return 1;
  }

  puts("hardware perf events deterministically unavailable");
  return 0;
}
