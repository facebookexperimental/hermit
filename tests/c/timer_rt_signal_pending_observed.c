/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A real-time POSIX timer expiration that every thread blocks
 * (https://github.com/rrnewton/hermit/issues/4089). Linux queues the signal,
 * and nothing changes until the process could observe it. Detcore cannot
 * deliver real-time signals, so it keeps such an expiration pending and refuses
 * the run by name only where it becomes observable; the process's exit, or a
 * change of the signal's disposition to SIG_IGN, discards it as Linux does, and
 * exec keeps it.
 *
 * Each case blocks SIGRTMIN+1, arms a 10 ms timer, sleeps past its expiry, and
 * then does one thing (argv[1]). It prints "expired" first, then "survived"
 * if the run goes on. A case that observes the signal under Linux would get it
 * here; under Hermit the run stops with the policy-refusal status before
 * "survived".
 */

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/signalfd.h>
#include <time.h>
#include <unistd.h>

static void sleep_past_the_deadline(void) {
  struct timespec past = {0, 50 * 1000 * 1000};
  nanosleep(&past, NULL);
}

int main(int argc, char** argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: %s CASE\n", argv[0]);
    return 2;
  }
  setvbuf(stdout, NULL, _IONBF, 0);
  const char* which = argv[1];
  int rt = SIGRTMIN + 1;
  sigset_t rt_only;
  sigemptyset(&rt_only);
  sigaddset(&rt_only, rt);

  if (strcmp(which, "after-exec-sigpending") == 0 ||
      strcmp(which, "after-exec-exit") == 0) {
    /* The new image: the pending signal and the blocked mask survived exec. */
    if (strcmp(which, "after-exec-sigpending") == 0) {
      sigset_t pending;
      sigpending(&pending);
    }
    printf("survived\n");
    return 0;
  }

  sigprocmask(SIG_BLOCK, &rt_only, NULL);
  struct sigevent event;
  memset(&event, 0, sizeof event);
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = rt;
  timer_t timer;
  if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
    perror("timer_create");
    return 1;
  }
  struct itimerspec soon;
  memset(&soon, 0, sizeof soon);
  soon.it_value.tv_nsec = 10 * 1000 * 1000;
  timer_settime(timer, 0, &soon, NULL);
  sleep_past_the_deadline();
  printf("expired\n");

  sigset_t empty;
  sigemptyset(&empty);
  if (strcmp(which, "unblock") == 0) {
    sigprocmask(SIG_UNBLOCK, &rt_only, NULL);
  } else if (strcmp(which, "setmask-empty") == 0) {
    sigprocmask(SIG_SETMASK, &empty, NULL);
  } else if (strcmp(which, "block-other") == 0) {
    sigset_t other;
    sigemptyset(&other);
    sigaddset(&other, SIGUSR1);
    sigprocmask(SIG_BLOCK, &other, NULL);
  } else if (strcmp(which, "sigpending") == 0) {
    sigset_t pending;
    sigpending(&pending);
  } else if (strcmp(which, "sigtimedwait") == 0) {
    struct timespec wait = {0, 1000 * 1000};
    siginfo_t info;
    sigtimedwait(&rt_only, &info, &wait);
  } else if (strcmp(which, "sigsuspend") == 0) {
    sigsuspend(&empty);
  } else if (strcmp(which, "signalfd") == 0) {
    signalfd(-1, &rt_only, 0);
  } else if (strcmp(which, "ppoll") == 0) {
    struct timespec zero = {0, 0};
    ppoll(NULL, 0, &zero, &empty);
  } else if (strcmp(which, "pselect") == 0) {
    struct timespec zero = {0, 0};
    pselect(0, NULL, NULL, NULL, &zero, &empty);
  } else if (strcmp(which, "epoll_pwait") == 0) {
    int epfd = epoll_create1(0);
    struct epoll_event events[1];
    epoll_pwait(epfd, events, 1, 0, &empty);
  } else if (strcmp(which, "ignore-then-unblock") == 0) {
    /* SIG_IGN flushes the pending signal (do_sigaction). */
    struct sigaction ignore;
    memset(&ignore, 0, sizeof ignore);
    ignore.sa_handler = SIG_IGN;
    sigaction(rt, &ignore, NULL);
    sigprocmask(SIG_UNBLOCK, &rt_only, NULL);
  } else if (strcmp(which, "exit") == 0) {
    /* The dying process discards it. */
  } else if (strcmp(which, "exec-sigpending") == 0) {
    execl(argv[0], argv[0], "after-exec-sigpending", (char*)NULL);
    perror("execl");
    return 1;
  } else if (strcmp(which, "exec-exit") == 0) {
    execl(argv[0], argv[0], "after-exec-exit", (char*)NULL);
    perror("execl");
    return 1;
  } else {
    fprintf(stderr, "unknown case %s\n", which);
    return 2;
  }
  printf("survived\n");
  return 0;
}
