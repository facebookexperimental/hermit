/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression probe for SIGEV_SIGNAL delivery from POSIX timers.
 *
 * Detcore currently tracks timer_create/timer_settime state under --strict,
 * but does not deliver the configured signal when the timer expires. Keep the
 * wait bounded: native Linux should handle SIGALRM after 10 ms and exit zero,
 * while the current strict Hermit run reaches the 100 ms virtual-time deadline
 * and reports the missing signal instead of hanging.
 */

#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

enum {
  TIMER_DELAY_NS = 10 * 1000 * 1000,
  WAIT_DEADLINE_NS = 100 * 1000 * 1000,
};

static volatile sig_atomic_t alarm_delivered;

static void alarm_handler(int signal_number) {
  (void)signal_number;
  alarm_delivered = 1;
}

static struct timespec add_nanoseconds(struct timespec time, long nanoseconds) {
  time.tv_nsec += nanoseconds;
  if (time.tv_nsec >= 1000 * 1000 * 1000) {
    ++time.tv_sec;
    time.tv_nsec -= 1000 * 1000 * 1000;
  }
  return time;
}

static long long elapsed_nanoseconds(struct timespec start,
                                     struct timespec finish) {
  return (long long)(finish.tv_sec - start.tv_sec) * 1000 * 1000 * 1000 +
         finish.tv_nsec - start.tv_nsec;
}

int main(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = alarm_handler;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return 2;
  }

  struct sigevent event;
  memset(&event, 0, sizeof(event));
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = SIGALRM;

  timer_t timer;
  if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
    perror("timer_create");
    return 3;
  }

  struct timespec start;
  if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
    perror("clock_gettime");
    return 4;
  }

  const struct itimerspec setting = {
      .it_value = {.tv_sec = 0, .tv_nsec = TIMER_DELAY_NS},
  };
  if (timer_settime(timer, 0, &setting, NULL) != 0) {
    perror("timer_settime");
    return 5;
  }

  const struct timespec deadline = add_nanoseconds(start, WAIT_DEADLINE_NS);
  while (!alarm_delivered) {
    const int result =
        clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &deadline, NULL);
    if (result == 0) {
      break;
    }
    if (result != EINTR) {
      errno = result;
      perror("clock_nanosleep");
      return 6;
    }
  }

  struct timespec finish;
  if (clock_gettime(CLOCK_MONOTONIC, &finish) != 0) {
    perror("clock_gettime");
    return 7;
  }

  if (timer_delete(timer) != 0) {
    perror("timer_delete");
    return 8;
  }

  const long long elapsed = elapsed_nanoseconds(start, finish);
  if (!alarm_delivered) {
    fputs("FAIL: SIGALRM was not delivered within 100 ms of virtual time\n",
          stderr);
    return 1;
  }
  if (elapsed < TIMER_DELAY_NS || elapsed > WAIT_DEADLINE_NS) {
    fprintf(stderr,
            "FAIL: SIGALRM was delivered outside the expected window\n");
    return 1;
  }

  puts("PASS: SIGALRM delivered after POSIX timer expiration");
  return 0;
}
