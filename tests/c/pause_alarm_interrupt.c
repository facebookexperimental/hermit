/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * `pause(2)` has no deadline: it returns only when a signal is delivered, and
 * then it always fails with EINTR. Detcore models it as a sleep until
 * `LogicalTime::INDEFINITE`, which the scheduler must never satisfy by
 * fast-forwarding virtual time -- only by delivering a signal.
 *
 * This guest is the positive half of that bracket: the single thread parks in
 * `pause()` with nothing else runnable, so the alarm is the only pending timed
 * event that can ever fire. The scheduler must advance virtual time to the
 * alarm's deadline (not past it to the end of logical time), deliver SIGALRM,
 * and resume `pause()` as an interruption.
 *
 * The negative half -- an indefinite wait with no signal that can ever arrive --
 * is a guest that never terminates, so it is covered by the scheduler unit tests
 * in detcore/src/scheduler.rs rather than by an e2e guest.
 */

#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static volatile sig_atomic_t handler_ran = 0;

static void on_alarm(int signo) {
  (void)signo;
  handler_ran = 1;
}

int main(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = on_alarm;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGALRM, &sa, NULL) != 0) {
    puts("PAUSE_ALARM_SIGACTION_FAILED");
    return 1;
  }

  alarm(1);

  errno = 0;
  int rc = pause();
  int saved_errno = errno;

  /* Report booleans rather than strerror(3) so the output cannot depend on the
   * ambient locale. */
  printf(
      "pause rc=%d eintr=%d handler_ran=%d\n",
      rc,
      saved_errno == EINTR,
      (int)handler_ran);

  if (rc != -1 || saved_errno != EINTR || handler_ran != 1) {
    puts("PAUSE_ALARM_INTERRUPT_FAILED");
    return 1;
  }

  puts("PAUSE_ALARM_INTERRUPT_OK");
  return 0;
}
