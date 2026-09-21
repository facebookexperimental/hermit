/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

#ifndef SCHED_DEADLINE
#define SCHED_DEADLINE 6
#endif

struct sched_attr_compat {
  uint32_t size;
  uint32_t sched_policy;
  uint64_t sched_flags;
  int32_t sched_nice;
  uint32_t sched_priority;
  uint64_t sched_runtime;
  uint64_t sched_deadline;
  uint64_t sched_period;
};

static int require_zero(long result, const char *name) {
  if (result != 0) {
    fprintf(stderr, "%s failed: %s\n", name, strerror(errno));
    return 1;
  }
  return 0;
}

int main(void) {
  struct itimerval timer;
  memset(&timer, 0, sizeof(timer));
  if (getitimer(ITIMER_REAL, &timer) != 0 || timer.it_value.tv_sec != 0 ||
      timer.it_value.tv_usec != 0 || timer.it_interval.tv_sec != 0 ||
      timer.it_interval.tv_usec != 0) {
    fputs("initial ITIMER_REAL was not disarmed\n", stderr);
    return 1;
  }

  timer.it_value.tv_sec = 1;
  if (setitimer(ITIMER_REAL, &timer, NULL) != 0) {
    perror("setitimer");
    return 1;
  }
  memset(&timer, 0, sizeof(timer));
  if (getitimer(ITIMER_REAL, &timer) != 0 ||
      (timer.it_value.tv_sec == 0 && timer.it_value.tv_usec == 0) ||
      timer.it_interval.tv_sec != 0 || timer.it_interval.tv_usec != 0) {
    fputs("logical ITIMER_REAL query lost the pending one-shot timer\n", stderr);
    return 1;
  }
  struct itimerval disarmed;
  memset(&disarmed, 0, sizeof(disarmed));
  if (setitimer(ITIMER_REAL, &disarmed, NULL) != 0) {
    perror("disarm setitimer");
    return 1;
  }

  long priority = syscall(SYS_ioprio_get, 1, 0);
  if (priority != 0) {
    fprintf(stderr, "ioprio_get returned %ld, expected virtual default 0\n",
            priority);
    return 1;
  }

  struct sched_attr_compat attr;
  memset(&attr, 0, sizeof(attr));
  attr.size = sizeof(attr);
  attr.sched_policy = SCHED_DEADLINE;
  attr.sched_runtime = 100000;
  attr.sched_deadline = 200000;
  attr.sched_period = 200000;
  if (require_zero(syscall(SYS_sched_setattr, 0, &attr, 0),
                   "sched_setattr") != 0) {
    return 1;
  }

  puts("scheduler-policy-queries-ok");
  return 0;
}
