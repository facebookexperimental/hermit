/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef HERMIT_TESTS_C_SCHED_SETATTR_COMMON_H
#define HERMIT_TESTS_C_SCHED_SETATTR_COMMON_H

#define _GNU_SOURCE

#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef TEST_SCHED_POLICY
#error "TEST_SCHED_POLICY must name the requested Linux scheduler policy"
#endif

#ifndef TEST_SCHED_LABEL
#error "TEST_SCHED_LABEL must name the requested Linux scheduler policy"
#endif

struct hermit_sched_attr {
  uint32_t size;
  uint32_t sched_policy;
  uint64_t sched_flags;
  int32_t sched_nice;
  uint32_t sched_priority;
  uint64_t sched_runtime;
  uint64_t sched_deadline;
  uint64_t sched_period;
  uint32_t sched_util_min;
  uint32_t sched_util_max;
};

int main(void) {
  struct hermit_sched_attr attr = {
      .size = sizeof(attr),
      .sched_policy = TEST_SCHED_POLICY,
  };

  if (syscall(SYS_sched_setattr, 0, &attr, 0) != 0) {
    perror("sched_setattr");
    return 1;
  }

  puts("sched_setattr: " TEST_SCHED_LABEL);
  return 0;
}

#endif
