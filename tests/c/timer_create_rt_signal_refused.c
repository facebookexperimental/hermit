/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * POSIX timers that notify with a real-time signal
 * (https://github.com/rrnewton/hermit/issues/3893). Detcore cannot deliver
 * real-time signals, and it used to arm such a timer that then silently never
 * fired. Now:
 *   1. creating, arming, reading and deleting one before it expires behaves as
 *      on Linux, so the first line is the same everywhere;
 *   1a. a process that exits or execs with one armed loses it, so nothing
 *      expires;
 *   1b. an expiration of an ignored, unblocked one is discarded, as Linux
 *      discards the signal, and the run goes on;
 *   2. when one expires, a fail-closed run stops with the policy-refusal status
 *      and names the signal; a run with --allow-unsupported-syscalls keeps the
 *      old behavior, so the wait times out (EAGAIN) where Linux delivers it;
 *   3. glibc's SIGEV_THREAD timers, which use SIGEV_THREAD_ID with an internal
 *      real-time signal, fire natively and never fire in the permissive run.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile int thread_fired;

static void on_expiry(union sigval value) {
  thread_fired = value.sival_int;
}

/* Arms a 10 ms SIGRTMIN+1 timer in this process. */
static void arm_soon(void) {
  struct sigevent event;
  memset(&event, 0, sizeof event);
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = SIGRTMIN + 1;
  timer_t timer;
  timer_create(CLOCK_MONOTONIC, &event, &timer);
  struct itimerspec soon;
  memset(&soon, 0, sizeof soon);
  soon.it_value.tv_nsec = 10 * 1000 * 1000;
  timer_settime(timer, 0, &soon, NULL);
}

static void sleep_past_the_deadline(void) {
  struct timespec past = {0, 50 * 1000 * 1000};
  nanosleep(&past, NULL);
}

int main(int argc, char** argv) {
  if (argc > 1 && strcmp(argv[1], "after-exec") == 0) {
    /* exec deleted the timer the old image armed. */
    sleep_past_the_deadline();
    return 0;
  }
  /* Unbuffered, so line 1 survives a refusal that ends the run at line 2. */
  setvbuf(stdout, NULL, _IONBF, 0);
  int rt = SIGRTMIN + 1;
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, rt);
  sigprocmask(SIG_BLOCK, &set, NULL);

  struct sigevent event;
  memset(&event, 0, sizeof event);
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = rt;
  event.sigev_value.sival_int = 42;

  /* 1. Armed far in the future, read back, deleted: never expires. */
  timer_t idle;
  if (timer_create(CLOCK_MONOTONIC, &event, &idle) != 0) {
    perror("timer_create");
    return 1;
  }
  struct itimerspec far;
  memset(&far, 0, sizeof far);
  far.it_value.tv_sec = 300;
  timer_settime(idle, 0, &far, NULL);
  struct itimerspec left;
  timer_gettime(idle, &left);
  printf(
      "unexpired SIGRTMIN+1 timer: armed %d, delete %d\n",
      left.it_value.tv_sec > 0,
      timer_delete(idle));

  /* A process that exits, or execs, with a real-time timer armed loses the
   * timer (exit_itimers; exec's posix timer deletion), so nothing expires. */
  pid_t child = fork();
  if (child == 0) {
    arm_soon();
    _exit(0);
  }
  int status;
  waitpid(child, &status, 0);
  sleep_past_the_deadline();
  printf("child exited with a SIGRTMIN+1 timer armed: status %d\n", WEXITSTATUS(status));
  child = fork();
  if (child == 0) {
    arm_soon();
    execl(argv[0], argv[0], "after-exec", (char*)NULL);
    _exit(9);
  }
  waitpid(child, &status, 0);
  printf("child execed with a SIGRTMIN+1 timer armed: status %d\n", WEXITSTATUS(status));

  /* An ignored, unblocked real-time signal: Linux discards each expiration
   * (sig_ignored), so the program runs on, natively and under Hermit. */
  struct sigaction ignore;
  memset(&ignore, 0, sizeof ignore);
  ignore.sa_handler = SIG_IGN;
  sigaction(SIGRTMIN + 2, &ignore, NULL);
  struct sigevent ignored_event = event;
  ignored_event.sigev_signo = SIGRTMIN + 2;
  timer_t ignored;
  if (timer_create(CLOCK_MONOTONIC, &ignored_event, &ignored) != 0) {
    perror("timer_create ignored");
    return 1;
  }
  struct itimerspec soon_ignored;
  memset(&soon_ignored, 0, sizeof soon_ignored);
  soon_ignored.it_value.tv_nsec = 10 * 1000 * 1000;
  timer_settime(ignored, 0, &soon_ignored, NULL);
  struct timespec past = {0, 50 * 1000 * 1000};
  nanosleep(&past, NULL);
  printf("ignored SIGRTMIN+2 timer expired: the run goes on\n");

  /* 2. Expires after 10 ms; wait for its signal for up to a second. */
  timer_t soon;
  if (timer_create(CLOCK_MONOTONIC, &event, &soon) != 0) {
    perror("timer_create");
    return 1;
  }
  struct itimerspec near;
  memset(&near, 0, sizeof near);
  near.it_value.tv_nsec = 10 * 1000 * 1000;
  timer_settime(soon, 0, &near, NULL);
  struct timespec wait = {1, 0};
  siginfo_t info;
  int got = sigtimedwait(&set, &info, &wait);
  if (got == rt) {
    printf("SIGRTMIN+1 timer expired: sival %d\n", info.si_value.sival_int);
  } else {
    printf("SIGRTMIN+1 timer expired: -1 %s\n", strerrorname_np(errno));
  }

  /* 3. A glibc SIGEV_THREAD timer that expires after 10 ms. */
  struct sigevent thread_event;
  memset(&thread_event, 0, sizeof thread_event);
  thread_event.sigev_notify = SIGEV_THREAD;
  thread_event.sigev_notify_function = on_expiry;
  thread_event.sigev_value.sival_int = 7;
  timer_t threaded;
  if (timer_create(CLOCK_MONOTONIC, &thread_event, &threaded) != 0) {
    perror("timer_create SIGEV_THREAD");
    return 1;
  }
  timer_settime(threaded, 0, &near, NULL);
  for (int i = 0; i < 100 && !thread_fired; i++) {
    usleep(10000);
  }
  printf("SIGEV_THREAD timer fired: %d\n", thread_fired);
  return 0;
}
