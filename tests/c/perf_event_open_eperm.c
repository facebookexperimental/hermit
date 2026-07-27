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

#ifndef SYS_perf_event_open
#define SYS_perf_event_open 298
#endif

int main(void) {
  struct perf_event_attr attr;
  memset(&attr, 0, sizeof(attr));
  attr.type = PERF_TYPE_SOFTWARE;
  attr.size = sizeof(attr);
  attr.config = PERF_COUNT_SW_CPU_CLOCK;
  attr.disabled = 1;

  errno = 0;
  long result = syscall(SYS_perf_event_open, &attr, 0, -1, -1, 0);
  if (result == -1 && errno == EPERM) {
    puts("perf_event_open deterministically refused");
    return 0;
  }

  if (result >= 0) {
    close((int)result);
  }
  fprintf(stderr,
          "perf_event_open: expected EPERM, got result=%ld errno=%d\n",
          result, errno);
  return 1;
}
