/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#246)

// RUN: %me | FileCheck %s
// CHECK: timerfd expirations=1 elapsed_ns={{[0-9]+}} requested_ns=10000000
// CHECK: timerfd respected clock deadline

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

#define TIMER_NS 10000000LL

static int64_t timespec_ns(struct timespec value) {
  return (int64_t)value.tv_sec * 1000000000LL + value.tv_nsec;
}

int main(void) {
  struct timespec before;
  if (clock_gettime(CLOCK_MONOTONIC, &before) != 0) {
    perror("clock_gettime before timerfd");
    return 1;
  }

  const int fd = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
  if (fd < 0) {
    perror("timerfd_create");
    return 1;
  }

  const struct itimerspec timer = {
      .it_value =
          {
              .tv_sec = 0,
              .tv_nsec = TIMER_NS,
          },
  };
  if (timerfd_settime(fd, 0, &timer, NULL) != 0) {
    perror("timerfd_settime");
    close(fd);
    return 1;
  }

  uint64_t expirations = 0;
  ssize_t bytes;
  do {
    bytes = read(fd, &expirations, sizeof(expirations));
  } while (bytes < 0 && errno == EINTR);
  if (bytes != (ssize_t)sizeof(expirations)) {
    fprintf(stderr, "timerfd read failed: bytes=%zd error=%s\n", bytes,
            strerror(errno));
    close(fd);
    return 1;
  }

  struct timespec after;
  if (clock_gettime(CLOCK_MONOTONIC, &after) != 0) {
    perror("clock_gettime after timerfd");
    close(fd);
    return 1;
  }
  close(fd);

  const int64_t elapsed_ns = timespec_ns(after) - timespec_ns(before);
  printf("timerfd expirations=%llu elapsed_ns=%lld requested_ns=%lld\n",
         (unsigned long long)expirations, (long long)elapsed_ns,
         (long long)TIMER_NS);

  if (expirations != 1) {
    fprintf(stderr, "timerfd expiration count mismatch: got=%llu expected=1\n",
            (unsigned long long)expirations);
    return 1;
  }
  if (elapsed_ns < TIMER_NS) {
    fprintf(stderr,
            "timerfd expired before its CLOCK_MONOTONIC deadline: "
            "elapsed_ns=%lld requested_ns=%lld\n",
            (long long)elapsed_ns, (long long)TIMER_NS);
    return 2;
  }

  puts("timerfd respected clock deadline");
  return 0;
}
