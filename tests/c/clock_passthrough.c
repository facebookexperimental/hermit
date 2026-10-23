/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Reads every clock syscall Detcore virtualizes, through raw syscalls so no
// vDSO path can bypass Hermit. Each value is printed and also passed as an
// lseek offset on /dev/null: replay re-emits the recording's stdout, but it
// compares syscall arguments, so a replay that observed a different value than
// the recording diverges at that lseek.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int witness_fd;

static void witness(const char* name, long long value) {
  printf("%s=%lld\n", name, value);
  fflush(stdout);
  if (lseek(witness_fd, (off_t)value, SEEK_SET) < 0) {
    fprintf(stderr, "lseek(%s) failed: %s\n", name, strerror(errno));
    _exit(1);
  }
}

static void check(const char* what, long result) {
  if (result != 0) {
    fprintf(stderr, "%s failed: %s\n", what, strerror(errno));
    _exit(1);
  }
}

static void read_clock(const char* name, clockid_t clock) {
  struct timespec ts;
  check(name, syscall(SYS_clock_gettime, clock, &ts));
  char label[64];
  snprintf(label, sizeof(label), "%s.sec", name);
  witness(label, ts.tv_sec);
  snprintf(label, sizeof(label), "%s.nsec", name);
  witness(label, ts.tv_nsec);
}

int main(void) {
  witness_fd = open("/dev/null", O_RDONLY);
  if (witness_fd < 0) {
    fprintf(stderr, "open(/dev/null) failed: %s\n", strerror(errno));
    return 1;
  }

  read_clock("realtime", CLOCK_REALTIME);
  read_clock("monotonic", CLOCK_MONOTONIC);
  read_clock("monotonic_raw", CLOCK_MONOTONIC_RAW);
  read_clock("boottime", CLOCK_BOOTTIME);

  struct timeval tv;
  check("gettimeofday", syscall(SYS_gettimeofday, &tv, NULL));
  witness("gettimeofday.sec", tv.tv_sec);
  witness("gettimeofday.usec", tv.tv_usec);

  time_t tloc = 0;
  long now = syscall(SYS_time, &tloc);
  if (now < 0 || now != tloc) {
    fprintf(stderr, "time returned %ld, stored %lld\n", now, (long long)tloc);
    return 1;
  }
  witness("time", now);

  struct timespec res;
  check("clock_getres", syscall(SYS_clock_getres, CLOCK_MONOTONIC, &res));
  witness("getres.sec", res.tv_sec);
  witness("getres.nsec", res.tv_nsec);
  // A NULL resolution pointer only validates the clock id.
  check("clock_getres(NULL)", syscall(SYS_clock_getres, CLOCK_REALTIME, NULL));

  // A NULL timespec pointer fails with EFAULT, and the recording logs that
  // failure as an event. The clock read after it checks that replay consumed
  // the failed call's event rather than handing it to the next syscall.
  errno = 0;
  long null_tp = syscall(SYS_clock_gettime, CLOCK_REALTIME, NULL);
  if (null_tp != -1 || errno != EFAULT) {
    fprintf(stderr, "clock_gettime(NULL) returned %ld: %s\n", null_tp, strerror(errno));
    return 1;
  }
  read_clock("after_null_tp", CLOCK_MONOTONIC);

  printf("clock-passthrough-ok\n");
  return 0;
}
