/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A periodic ITIMER_REAL with an empty handler over a CPU-bound loop that
 * makes no syscalls (https://github.com/rrnewton/hermit/issues/4139).
 * argv[1] is the period in microseconds, argv[2] the loop iterations in
 * millions. Prints the signals received and the CLOCK_MONOTONIC time the loop
 * took.
 */

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>

static volatile long signals = 0;

static void on_alarm(int signal) {
  (void)signal;
  signals++;
}

int main(int argc, char** argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s <period_us> <millions_of_iterations>\n", argv[0]);
    return 2;
  }
  long period_us = atol(argv[1]);
  long iterations = atol(argv[2]) * 1000000L;
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_alarm;
  action.sa_flags = SA_RESTART;
  sigaction(SIGALRM, &action, NULL);
  struct timespec start, end;
  clock_gettime(CLOCK_MONOTONIC, &start);
  struct itimerval timer = {
      {period_us / 1000000, period_us % 1000000},
      {period_us / 1000000, period_us % 1000000}};
  setitimer(ITIMER_REAL, &timer, NULL);
  volatile long acc = 0;
  for (long i = 0; i < iterations; i++) {
    acc += i & 7;
  }
  struct itimerval off = {{0, 0}, {0, 0}};
  setitimer(ITIMER_REAL, &off, NULL);
  clock_gettime(CLOCK_MONOTONIC, &end);
  long virtual_us =
      (end.tv_sec - start.tv_sec) * 1000000L + (end.tv_nsec - start.tv_nsec) / 1000;
  printf("period_us=%ld signals=%ld virtual_us=%ld\n", period_us, (long)signals, virtual_us);
  return 0;
}
