/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>

static volatile sig_atomic_t deliveries;

static void on_alarm(int signal_number) {
  (void)signal_number;
  ++deliveries;
}

int main(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_alarm;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }

  const struct itimerval periodic = {
      .it_interval = {.tv_sec = 0, .tv_usec = 10000},
      .it_value = {.tv_sec = 0, .tv_usec = 10000},
  };
  if (setitimer(ITIMER_REAL, &periodic, NULL) != 0) {
    perror("setitimer");
    return 2;
  }

  const struct timespec wait = {.tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
  for (int attempts = 0; deliveries < 3 && attempts < 10; ++attempts) {
    if (nanosleep(&wait, NULL) != 0 && errno != EINTR) {
      perror("nanosleep");
      return 3;
    }
  }

  const struct itimerval disarmed = {0};
  if (setitimer(ITIMER_REAL, &disarmed, NULL) != 0) {
    perror("disarm setitimer");
    return 4;
  }
  if (deliveries != 3) {
    fprintf(stderr, "expected 3 periodic alarms, got %d\n", (int)deliveries);
    return 5;
  }

  puts("PASS: 3 periodic ITIMER_REAL alarms delivered");
  return 0;
}
