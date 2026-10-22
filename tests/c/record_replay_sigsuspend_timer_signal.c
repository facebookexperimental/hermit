/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A timer's SIGALRM must not reach a thread on its way into rt_sigsuspend
 * (https://github.com/rrnewton/hermit/issues/3993).
 *
 * The main thread catches SIGALRM and SIGUSR1 and blocks SIGUSR1. In each of
 * eight rounds it arms a 100 ms ITIMER_REAL, starts a waker thread that sends
 * it SIGUSR1 after 300 ms, and calls sigsuspend with a mask that blocks only
 * SIGALRM. Linux gives the process-directed SIGALRM to the waker, the only
 * thread that does not block it (its usleep ends early), and SIGUSR1 then
 * ends the sigsuspend with EINTR, its handler run under the temporary mask.
 * Every round prints r=-1 errno=4 usr1=1 alarms=1.
 *
 * Hermit released the thread into the call and went on to the timer wake at
 * once, so the SIGALRM could reach the main thread under its own mask before
 * its rt_sigsuspend ran, as host timing decided. The call then never ran, its
 * SIGUSR1 stayed pending into the next round, and hermit run and record
 * printed usr1=0 in some rounds, and replay refused the recording.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

static volatile sig_atomic_t alarms;
static volatile sig_atomic_t usr1s;
static pthread_t main_thread;

static void on_alarm(int signal) {
  (void)signal;
  alarms++;
}

static void on_usr1(int signal) {
  (void)signal;
  usr1s++;
}

static void* waker(void* arg) {
  (void)arg;
  usleep(300000);
  pthread_kill(main_thread, SIGUSR1);
  return NULL;
}

int main(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_alarm;
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction SIGALRM");
    return 1;
  }
  action.sa_handler = on_usr1;
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    perror("sigaction SIGUSR1");
    return 1;
  }
  main_thread = pthread_self();
  sigset_t usr1_set;
  sigemptyset(&usr1_set);
  sigaddset(&usr1_set, SIGUSR1);
  if (pthread_sigmask(SIG_BLOCK, &usr1_set, NULL) != 0) {
    perror("pthread_sigmask");
    return 1;
  }
  int failures = 0;
  for (int round = 0; round < 8; round++) {
    alarms = 0;
    usr1s = 0;
    struct itimerval alarm_in = {{0, 0}, {0, 100000}};
    if (setitimer(ITIMER_REAL, &alarm_in, NULL) != 0) {
      perror("setitimer");
      return 1;
    }
    pthread_t thread;
    if (pthread_create(&thread, NULL, waker, NULL) != 0) {
      perror("pthread_create");
      return 1;
    }
    sigset_t during;
    sigemptyset(&during);
    sigaddset(&during, SIGALRM);
    errno = 0;
    int result = sigsuspend(&during);
    int result_errno = errno;
    if (pthread_join(thread, NULL) != 0) {
      perror("pthread_join");
      return 1;
    }
    printf(
        "round=%d r=%d errno=%d usr1=%d alarms=%d\n",
        round,
        result,
        result_errno,
        (int)usr1s,
        (int)alarms);
    if (result != -1 || result_errno != EINTR || usr1s != 1 || alarms != 1) {
      failures++;
    }
  }
  if (failures != 0) {
    fprintf(stderr, "sigsuspend timer signal: %d rounds differ from Linux\n", failures);
    return 1;
  }
  return 0;
}
