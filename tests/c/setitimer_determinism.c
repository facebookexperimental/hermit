/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Exercises the BSD interval-timer API setitimer(ITIMER_REAL) together with a
 * sigaction(SIGALRM) handler. This is the classic profiling/benchmark timer
 * (getitimer/setitimer with ITIMER_REAL / ITIMER_VIRTUAL / ITIMER_PROF).
 *
 * The program arms a 10ms repeating ITIMER_REAL timer, then spends a fixed
 * ~200ms window (twenty 10ms sleeps) during which SIGALRM should fire roughly
 * every 10ms. It counts the deliveries and prints the total.
 *
 * The strict timer path delivers SIGALRM from the deterministic virtual clock.
 * The fixed sequence above produces twenty deliveries under Hermit; native
 * delivery counts remain subject to host scheduling. A zero-delivery repeat is
 * not evidence that this timer works.
 *
 * The program still exits 0 and prints the observed count on the last line as
 *   "SIGALRM deliveries: <N>"
 * so its callers must separately assert positive delivery (and the exact count
 * for a fixed strict schedule), in addition to full verification. This keeps
 * the existing program instructions and wrapper assertions unchanged.
 */

#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

static volatile sig_atomic_t deliveries = 0;

static void on_alarm(int signo) {
  (void)signo;
  deliveries++;
}

int main(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = on_alarm;
  sigemptyset(&sa.sa_mask);
  /* No SA_RESTART: let a delivery interrupt the sleep, like a real profiler. */
  if (sigaction(SIGALRM, &sa, NULL) != 0) {
    perror("sigaction");
    return 1;
  }

  struct itimerval it;
  it.it_interval.tv_sec = 0;
  it.it_interval.tv_usec = 10000; /* repeat every 10ms */
  it.it_value.tv_sec = 0;
  it.it_value.tv_usec = 10000; /* first expiration at 10ms */
  if (setitimer(ITIMER_REAL, &it, NULL) != 0) {
    perror("setitimer");
    return 2;
  }

  /* Confirm getitimer round-trips the arming we just set. */
  struct itimerval got;
  memset(&got, 0, sizeof(got));
  if (getitimer(ITIMER_REAL, &got) != 0) {
    perror("getitimer");
    return 3;
  }
  int armed = (got.it_value.tv_sec > 0 || got.it_value.tv_usec > 0 ||
               got.it_interval.tv_usec > 0);
  printf("timer armed (getitimer remaining>0 or interval set): %d\n", armed);

  /* Fixed observation window: twenty 10ms sleeps (~200ms of virtual time).
   * usleep may return early on EINTR when SIGALRM fires; that is fine -- we
   * only care about the number of deliveries observed across the window. */
  for (int i = 0; i < 20; i++) {
    usleep(10000);
  }

  /* Disarm so nothing fires during teardown. */
  struct itimerval off;
  memset(&off, 0, sizeof(off));
  setitimer(ITIMER_REAL, &off, NULL);

  printf("SIGALRM deliveries: %d\n", (int)deliveries);
  return 0;
}
