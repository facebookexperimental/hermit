/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * sleep_signal_after_deadline: the main thread sleeps 2 ms with a SIGUSR1
 * handler while a helper thread, which blocks SIGUSR1, keeps making system
 * calls (sched_yield, each a scheduler turn) until the sleep is over. Under `hermit run --signal-at
 * turn=N:SIGUSR1` the root process gets SIGUSR1 after scheduler turn N, and
 * only the sleeper can take it. Linux ends the sleep with EINTR, and some
 * time left, only for a signal that arrives before the deadline; one that
 * arrives later finds the timer fired, and the sleep returns 0 with the
 * handler run as the call returns (do_nanosleep in kernel/time/hrtimer.c).
 *
 * Output, one line:
 *   "rc=0 before=B handled=H"
 *   "rc=EINTR rem=NS before=B handled=H"
 * where B is whether the handler ran before the sleep began and H whether it
 * has run by the time the line is printed.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled = 0;
static atomic_int done = 0;

static void on_usr1(int sig) {
  (void)sig;
  handled = 1;
}

static void* busy(void* arg) {
  (void)arg;
  /* Bounded, so that a sleep that never ends cannot hang the run. */
  for (long i = 0; i < 200000 && !atomic_load(&done); i++) {
    sched_yield();
  }
  return NULL;
}

int main(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_usr1;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR1, &sa, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  /* The helper inherits a mask that blocks SIGUSR1, so the process-directed
   * signal can only go to the sleeper. */
  pthread_sigmask(SIG_BLOCK, &usr1, NULL);
  pthread_t helper;
  int error = pthread_create(&helper, NULL, busy, NULL);
  if (error != 0) {
    fprintf(stderr, "pthread_create: %s\n", strerror(error));
    return 1;
  }
  pthread_sigmask(SIG_UNBLOCK, &usr1, NULL);
  int before = handled;
  struct timespec req = {0, 2000000};
  struct timespec rem = {0, 0};
  int rc = nanosleep(&req, &rem);
  int err = errno;
  atomic_store(&done, 1);
  pthread_join(helper, NULL);
  if (rc == 0) {
    printf("rc=0 before=%d handled=%d\n", before, (int)handled);
  } else if (err == EINTR) {
    printf(
        "rc=EINTR rem=%ld before=%d handled=%d\n",
        rem.tv_sec * 1000000000L + rem.tv_nsec,
        before,
        (int)handled);
  } else {
    printf("rc=%s\n", strerror(err));
  }
  return 0;
}
