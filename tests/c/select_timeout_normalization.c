/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// select(2)'s timeout as Linux v7.1 reads it: kern_select (fs/select.c)
// refuses a negative tv_sec or tv_usec, then folds tv_usec >= 1000000 into the
// seconds before poll_select_set_timeout. (v6.12 has no sign check, so its
// "mixed" lines differ: {2, -1000000} and {-1, 2000000} each wait 1 s there.) pselect6's timespec has no such folding: a
// tv_nsec of 1e9 or more is EINVAL (timespec64_valid). Each line is what the
// guest observes, natively and under Hermit alike.

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

static void report(const char* label, long result, int error, const struct timeval* tv) {
  printf("%s: r=%ld", label, result);
  if (result < 0)
    printf(" errno=%s", strerror(error));
  if (tv != NULL)
    printf(" tv=%ld.%06ld", (long)tv->tv_sec, (long)tv->tv_usec);
  printf("\n");
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);

  // 1.5 s spelled entirely in microseconds: a full timeout, then nothing left.
  struct timeval tv = {0, 1500000};
  double start = now();
  errno = 0;
  long result = syscall(SYS_select, 0, NULL, NULL, NULL, &tv);
  int error = errno;
  report("usec 1500000 times out", result, error, &tv);
  printf("waited at least 1.5 s: %d\n", now() - start >= 1.5);

  // A ready descriptor with a folded timeout still returns at once.
  int fds[2];
  if (pipe(fds) != 0 || write(fds[1], "x", 1) != 1)
    return 90;
  fd_set readfds;
  FD_ZERO(&readfds);
  FD_SET(fds[0], &readfds);
  struct timeval long_tv = {0, 2000000};
  errno = 0;
  result = syscall(SYS_select, fds[0] + 1, &readfds, NULL, NULL, &long_tv);
  error = errno;
  report("usec 2000000 with a ready pipe", result, error, NULL);
  printf("ready descriptor reported: %d\n", result == 1 && FD_ISSET(fds[0], &readfds));

  struct timeval negative_usec = {0, -1};
  errno = 0;
  result = syscall(SYS_select, 0, NULL, NULL, NULL, &negative_usec);
  report("usec -1", result, errno, NULL);

  struct timeval negative_sec = {-1, 0};
  errno = 0;
  result = syscall(SYS_select, 0, NULL, NULL, NULL, &negative_sec);
  report("sec -1", result, errno, NULL);

  struct timeval mixed[] = {{2, -1000000}, {-1, 2000000}, {1, -1}};
  for (unsigned i = 0; i < sizeof(mixed) / sizeof(mixed[0]); i++) {
    char label[64];
    snprintf(label, sizeof(label), "mixed {%ld, %ld}", (long)mixed[i].tv_sec,
             (long)mixed[i].tv_usec);
    errno = 0;
    result = syscall(SYS_select, 0, NULL, NULL, NULL, &mixed[i]);
    report(label, result, errno, NULL);
  }

  struct timespec nsec_overflow = {0, 1000000000};
  errno = 0;
  result = syscall(SYS_pselect6, 0, NULL, NULL, NULL, &nsec_overflow, NULL);
  report("pselect6 nsec 1000000000", result, errno, NULL);
  return 0;
}
