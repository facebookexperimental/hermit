/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * signal_at_turn: the root process counts 2000 sched_yield calls and notes at
 * which one SIGUSR1 first arrived (-1 if it never did). Under
 * `hermit run --signal-at turn=N:SIGUSR1` the signal is sent after scheduler
 * turn N, so the noted iteration is the same in every run with the same N.
 *
 * Output: "signal at iteration <i>"
 */

#include <sched.h>
#include <signal.h>
#include <stdio.h>

static volatile sig_atomic_t seen = -1;
static volatile long iteration;

static void on_usr1(int signo) {
  (void)signo;
  if (seen < 0) {
    seen = (sig_atomic_t)iteration;
  }
}

int main(void) {
  if (signal(SIGUSR1, on_usr1) == SIG_ERR) {
    return 2;
  }
  for (iteration = 0; iteration < 2000; iteration++) {
    sched_yield();
  }
  printf("signal at iteration %ld\n", (long)seen);
  return 0;
}
