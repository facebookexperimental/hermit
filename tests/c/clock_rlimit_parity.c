/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Resource-limit and clock answers that two backends sharing Detcore must give
// identically.
//
//   limits      prints getrlimit and prlimit64(0, ...) for every resource,
//               the errors for an unknown resource and a raised hard limit, a
//               lowered soft limit read back, and clock_getres and the
//               invalid-clock error for each clock.
//   vfork-clock reads CLOCK_MONOTONIC with a raw syscall in a vfork child
//               before it exits, and prints whether the read succeeded.

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void limits(void) {
  for (int resource = 0; resource < RLIM_NLIMITS; resource++) {
    struct rlimit get = {0, 0};
    struct rlimit pr = {0, 0};
    int get_rc = getrlimit(resource, &get);
    int pr_rc = prlimit(0, resource, NULL, &pr);
    printf(
        "rlimit %d: get=%d %llx %llx prlimit=%d %llx %llx\n",
        resource,
        get_rc,
        (unsigned long long)get.rlim_cur,
        (unsigned long long)get.rlim_max,
        pr_rc,
        (unsigned long long)pr.rlim_cur,
        (unsigned long long)pr.rlim_max);
  }
  // Each call runs before its errno is read: printf's arguments are evaluated
  // in an unspecified order.
  struct rlimit value;
  errno = 0;
  int unknown_rc = getrlimit(RLIM_NLIMITS + 3, &value);
  printf("rlimit unknown: %d %s\n", unknown_rc, strerrorname_np(errno));

  struct rlimit nofile;
  getrlimit(RLIMIT_NOFILE, &nofile);
  struct rlimit raised = nofile;
  if (raised.rlim_max != RLIM_INFINITY) {
    raised.rlim_max += 1;
    raised.rlim_cur = raised.rlim_max;
    errno = 0;
    int raise_rc = setrlimit(RLIMIT_NOFILE, &raised);
    printf("rlimit raise hard: %d %s\n", raise_rc, strerrorname_np(errno));
  }
  struct rlimit lowered = {64, nofile.rlim_max};
  int set_rc = setrlimit(RLIMIT_NOFILE, &lowered);
  struct rlimit back = {0, 0};
  getrlimit(RLIMIT_NOFILE, &back);
  printf(
      "rlimit lowered: %d %llx %llx\n",
      set_rc,
      (unsigned long long)back.rlim_cur,
      (unsigned long long)back.rlim_max);

  const clockid_t clocks[] = {
      CLOCK_REALTIME,
      CLOCK_MONOTONIC,
      CLOCK_PROCESS_CPUTIME_ID,
      CLOCK_THREAD_CPUTIME_ID,
      CLOCK_MONOTONIC_RAW,
      CLOCK_REALTIME_COARSE,
      CLOCK_MONOTONIC_COARSE,
      CLOCK_BOOTTIME,
  };
  for (size_t i = 0; i < sizeof(clocks) / sizeof(clocks[0]); i++) {
    struct timespec resolution = {0, 0};
    long rc = syscall(SYS_clock_getres, clocks[i], &resolution);
    printf(
        "clock_getres %d: %ld %lld.%09ld\n",
        (int)clocks[i],
        rc,
        (long long)resolution.tv_sec,
        resolution.tv_nsec);
  }
  struct timespec ignored;
  errno = 0;
  long rc = syscall(SYS_clock_gettime, (clockid_t)1000, &ignored);
  printf("clock_gettime invalid: %ld %s\n", rc, strerrorname_np(errno));
}

static void vfork_clock(void) {
  volatile long child_rc = -2;
  pid_t child = vfork();
  if (child == 0) {
    struct timespec now;
    child_rc = syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &now);
    _exit(0);
  }
  int status = 0;
  waitpid(child, &status, 0);
  printf(
      "vfork child clock_gettime: %s, exit %d\n",
      child_rc == 0 ? "ok" : "failed",
      WIFEXITED(status) ? WEXITSTATUS(status) : -1);
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "limits") == 0) {
    limits();
    return 0;
  }
  if (argc == 2 && strcmp(argv[1], "vfork-clock") == 0) {
    vfork_clock();
    return 0;
  }
  fprintf(stderr, "usage: %s limits|vfork-clock\n", argv[0]);
  return 2;
}
