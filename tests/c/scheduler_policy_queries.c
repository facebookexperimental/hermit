/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
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

static int check_scheduler_queries(void) {
  int policy = sched_getscheduler(0);
  struct sched_param parameter;
  memset(&parameter, 0, sizeof(parameter));
  if (policy != SCHED_OTHER || sched_getparam(0, &parameter) != 0 ||
      parameter.sched_priority != 0) {
    fprintf(stderr, "legacy scheduler query mismatch: policy=%d priority=%d\n",
            policy, parameter.sched_priority);
    return 1;
  }

  struct sched_attr_compat queried;
  memset(&queried, 0, sizeof(queried));
  queried.size = sizeof(queried);
  if (syscall(SYS_sched_getattr, 0, &queried, sizeof(queried), 0) != 0 ||
      queried.sched_policy != (uint32_t)policy ||
      queried.sched_priority != (uint32_t)parameter.sched_priority) {
    fprintf(stderr, "sched_getattr mismatch: policy=%u priority=%u errno=%d\n",
            queried.sched_policy, queried.sched_priority, errno);
    return 1;
  }

  int priority_min = sched_get_priority_min(SCHED_OTHER);
  int priority_max = sched_get_priority_max(SCHED_OTHER);
  if (priority_min != 0 || priority_max != 0) {
    fprintf(stderr, "SCHED_OTHER range mismatch: min=%d max=%d\n", priority_min,
            priority_max);
    return 1;
  }

  cpu_set_t affinity;
  CPU_ZERO(&affinity);
  if (sched_getaffinity(0, sizeof(affinity), &affinity) != 0 ||
      CPU_COUNT(&affinity) == 0) {
    fprintf(stderr, "sched_getaffinity returned an empty mask: %s\n",
            strerror(errno));
    return 1;
  }

  struct timespec interval;
  memset(&interval, 0, sizeof(interval));
  if (sched_rr_get_interval(0, &interval) != 0 || interval.tv_sec < 0 ||
      interval.tv_nsec < 0 || interval.tv_nsec >= 1000000000L) {
    fprintf(stderr, "invalid sched_rr_get_interval result: %ld.%09ld\n",
            (long)interval.tv_sec, interval.tv_nsec);
    return 1;
  }

  long nice_raw = syscall(SYS_getpriority, PRIO_PROCESS, 0);
  long nice_repeat = syscall(SYS_getpriority, PRIO_PROCESS, 0);
  if (nice_raw < 1 || nice_raw > 40 || nice_repeat != nice_raw) {
    fprintf(stderr, "getpriority returned unstable raw values: %ld/%ld\n",
            nice_raw, nice_repeat);
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
    fputs("logical ITIMER_REAL query lost the pending one-shot timer\n",
          stderr);
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

  if (check_scheduler_queries() != 0) {
    return 1;
  }

  struct sched_attr_compat attr;
  memset(&attr, 0, sizeof(attr));
  attr.size = sizeof(attr);
  attr.sched_policy = SCHED_DEADLINE;
  attr.sched_runtime = 100000;
  attr.sched_deadline = 200000;
  attr.sched_period = 200000;
  if (require_zero(syscall(SYS_sched_setattr, 0, &attr, 0), "sched_setattr") !=
      0) {
    return 1;
  }

  puts("scheduler-policy-queries-ok");
  return 0;
}
