/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <linux/hw_breakpoint.h>
#include <linux/perf_event.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
  uint64_t watched = 0;
  struct perf_event_attr attr = {
      .type = PERF_TYPE_BREAKPOINT,
      .size = sizeof(attr),
      .bp_type = HW_BREAKPOINT_W,
      .bp_addr = (uintptr_t)&watched,
      .bp_len = HW_BREAKPOINT_LEN_8,
      .disabled = 1,
      .exclude_kernel = 1,
      .exclude_hv = 1,
  };

  errno = 0;
  long result = syscall(SYS_perf_event_open, &attr, 0, -1, -1, 0);
  if (result != -1 || errno != ENOSYS) {
    fprintf(stderr,
            "watchpoint perf_event_open returned %ld with errno %d (%s), "
            "expected ENOSYS\n",
            result, errno, strerror(errno));
    if (result >= 0) {
      close((int)result);
    }
    return 1;
  }

  puts("watchpoint perf events deterministically unavailable");
  return 0;
}
