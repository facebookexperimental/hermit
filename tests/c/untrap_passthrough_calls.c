/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * untrap_passthrough_calls N
 *
 * Makes each of the seven syscalls --untrap-passthrough-syscalls leaves to the
 * kernel (getppid, umask, getgroups, capget, sched_get_priority_max,
 * sched_get_priority_min and get_robust_list) N times, between two
 * CLOCK_MONOTONIC reads, through syscall(2) so that no C library caches a
 * result. It prints each call's last result, which is the same with the flag
 * and without, and then the interval between the two reads, which under
 * Hermit is virtual time: with the flag the calls charge none.
 */

#define _GNU_SOURCE
#include <linux/capability.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s N\n", argv[0]);
    return 2;
  }
  long n = atol(argv[1]);
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct caps[2];
  gid_t groups[256];
  void *robust_head = NULL;
  size_t robust_length = 0;
  long ppid = 0, mask = 0, ngroups = 0, capget = 0, max = 0, min = 0,
       robust = 0;
  struct timespec start, end;

  if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
    perror("clock_gettime");
    return 1;
  }
  for (long i = 0; i < n; i++) {
    ppid = syscall(SYS_getppid);
    mask = syscall(SYS_umask, 022);
    ngroups = syscall(SYS_getgroups, 256, groups);
    capget = syscall(SYS_capget, &header, caps);
    max = syscall(SYS_sched_get_priority_max, SCHED_FIFO);
    min = syscall(SYS_sched_get_priority_min, SCHED_FIFO);
    robust = syscall(SYS_get_robust_list, 0, &robust_head, &robust_length);
  }
  if (clock_gettime(CLOCK_MONOTONIC, &end) != 0) {
    perror("clock_gettime");
    return 1;
  }
  printf("getppid %ld umask %lo getgroups %ld capget %ld "
         "sched_get_priority_max %ld sched_get_priority_min %ld "
         "get_robust_list %ld\n",
         ppid, mask, ngroups, capget, max, min, robust);
  printf("interval %lld ns\n",
         (long long)(end.tv_sec - start.tv_sec) * 1000000000LL +
             (end.tv_nsec - start.tv_nsec));
  return 0;
}
