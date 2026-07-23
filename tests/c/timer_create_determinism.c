/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Exercises the POSIX per-process timer family that Detcore emulates under
 * --strict: timer_create / timer_settime / timer_gettime / timer_delete.
 *
 * The timer is armed for a long interval so it never fires within the run
 * (Detcore tracks a timer's arming against the virtual clock but does not
 * deliver expiration signals). The remaining time reported by timer_gettime is
 * therefore driven by the deterministic virtual clock and must be reproducible.
 */

#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

int main(void) {
  timer_t timerid;
  struct sigevent sev;
  memset(&sev, 0, sizeof(sev));
  sev.sigev_notify = SIGEV_SIGNAL;
  sev.sigev_signo = SIGRTMIN;

  if (timer_create(CLOCK_MONOTONIC, &sev, &timerid) != 0) {
    perror("timer_create");
    return 1;
  }

  struct itimerspec its;
  memset(&its, 0, sizeof(its));
  its.it_value.tv_sec = 300; /* long enough to never fire during the test */

  struct itimerspec old;
  memset(&old, 0, sizeof(old));
  if (timer_settime(timerid, 0, &its, &old) != 0) {
    perror("timer_settime");
    return 2;
  }
  /* The timer was previously disarmed, so the old value must be zero. */
  if (old.it_value.tv_sec != 0 || old.it_value.tv_nsec != 0) {
    puts("Error: old value of a freshly created timer should be zero");
    return 3;
  }

  struct itimerspec cur;
  memset(&cur, 0, sizeof(cur));
  if (timer_gettime(timerid, &cur) != 0) {
    perror("timer_gettime");
    return 4;
  }
  /* Some time remains; report only a stable predicate, not raw ns. */
  int armed = (cur.it_value.tv_sec > 0 || cur.it_value.tv_nsec > 0);
  printf("timer armed, remaining>0: %d\n", armed);

  if (timer_delete(timerid) != 0) {
    perror("timer_delete");
    return 5;
  }

  puts("timer_create determinism test ok");
  return 0;
}
