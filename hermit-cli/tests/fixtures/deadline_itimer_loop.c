/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// https://github.com/rrnewton/hermit/issues/4121: periodic timers and the cap.
// Periodic ITIMER_REAL (argv[1] us) while the main thread makes argv[2] (thousands)
// getppid calls. Prints signals delivered and CLOCK_MONOTONIC virtual time.
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>
static volatile long sigs = 0;
static void on_alrm(int s) { (void)s; sigs++; }
int main(int argc, char **argv) {
  long us = argc > 1 ? atol(argv[1]) : 1000;
  long calls = (argc > 2 ? atol(argv[2]) : 100) * 1000L;
  struct sigaction sa; memset(&sa, 0, sizeof sa); sa.sa_handler = on_alrm; sa.sa_flags = SA_RESTART;
  sigaction(SIGALRM, &sa, NULL);
  struct timespec t0, t1; clock_gettime(CLOCK_MONOTONIC, &t0);
  struct itimerval it = { { us / 1000000, us % 1000000 }, { us / 1000000, us % 1000000 } };
  setitimer(ITIMER_REAL, &it, NULL);
  for (long i = 0; i < calls; i++) syscall(SYS_getppid);
  struct itimerval off = {{0,0},{0,0}}; setitimer(ITIMER_REAL, &off, NULL);
  clock_gettime(CLOCK_MONOTONIC, &t1);
  double ms = (t1.tv_sec - t0.tv_sec) * 1e3 + (t1.tv_nsec - t0.tv_nsec) / 1e6;
  printf("interval_us=%ld calls=%ld signals=%ld virtual_ms=%.3f\n", us, calls, (long)sigs, ms);
  return 0;
}
