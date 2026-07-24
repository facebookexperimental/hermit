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
 * WHAT THIS DEMONSTRATES / GAP EXPOSED
 *   Detcore has no dedicated setitimer/getitimer handler. Under --strict the
 *   syscall is not rejected (no "unsupported syscall" abort), but the timer's
 *   expiration signal (SIGALRM) is NOT delivered against the virtual clock --
 *   exactly the same limitation Detcore documents for the POSIX per-process
 *   timer family (timer_create: "arming tracked against the virtual clock but
 *   expiration signals are not delivered"). So the guest observes ZERO SIGALRM
 *   deliveries under --strict, versus ~10-20 when run natively.
 *
 *   The zero-delivery outcome is itself DETERMINISTIC (it reproduces run to run
 *   and passes `--verify`), so this is a functional-completeness gap, not a
 *   nondeterminism bug: setitimer silently fails to fire rather than firing at
 *   an uncontrolled host-timed moment.
 *
 * CONTRACT
 *   Native:        deliveries >= 1  (timer fires; exact count host-timing bound)
 *   Hermit --strict (today): deliveries == 0, deterministically.
 *   Hermit --strict (goal):  deliveries deterministic and > 0, driven by the
 *                            virtual clock, matching the arming period.
 *
 * The program always exits 0 and prints the observed count on the last line as
 *   "SIGALRM deliveries: <N>"
 * so a wrapper (see tests/standalone/strict_setitimer.sh) can compare the
 * native and --strict counts and assert the determinism verdict.
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
