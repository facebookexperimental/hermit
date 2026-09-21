/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

static int is_zero(struct timeval value) {
  return value.tv_sec == 0 && value.tv_usec == 0;
}

int main(void) {
  struct itimerval current;
  memset(&current, 0xff, sizeof(current));
  if (getitimer(ITIMER_REAL, &current) != 0 || !is_zero(current.it_interval) ||
      !is_zero(current.it_value)) {
    perror("initial getitimer");
    return 1;
  }

  struct itimerval armed = {
      .it_interval = {0, 0},
      .it_value = {5, 0},
  };
  if (setitimer(ITIMER_REAL, &armed, NULL) != 0) {
    perror("setitimer");
    return 2;
  }
  if (getitimer(ITIMER_REAL, &current) != 0 || !is_zero(current.it_interval) ||
      is_zero(current.it_value) ||
      current.it_value.tv_sec > armed.it_value.tv_sec) {
    perror("armed getitimer");
    return 3;
  }

  struct itimerval disarmed = {{0, 0}, {0, 0}};
  if (setitimer(ITIMER_REAL, &disarmed, NULL) != 0 ||
      getitimer(ITIMER_REAL, &current) != 0 || !is_zero(current.it_interval) ||
      !is_zero(current.it_value)) {
    perror("disarmed getitimer");
    return 4;
  }

  errno = 0;
  if (syscall(SYS_getitimer, 99, &current) != -1 || errno != EINVAL) {
    fprintf(stderr, "invalid getitimer: expected EINVAL, got errno=%d\n",
            errno);
    return 5;
  }

  puts("getitimer-deterministic-ok");
  return 0;
}
