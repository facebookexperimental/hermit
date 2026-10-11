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
 * Most cases block SIGRTMIN+1, arm a 10 ms timer, sleep past its expiry, and
 * then do one thing (argv[1]). They print "expired" first, then "survived" if
 * the run goes on. A case that observes the signal under Linux would get it
 * there; under Hermit the run stops with the policy-refusal status before
 * "survived".
 *
 * The cases before that prelude are delivered at the expiry itself, on Linux
 * too, so under Hermit they stop before printing anything (or, for the
 * handler, as it returns).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
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

static void arm_soon(int signo) {
  struct sigevent event;
  memset(&event, 0, sizeof event);
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = signo;
  timer_t timer;
  if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
    perror("timer_create");
    _exit(1);
  }
  struct itimerspec soon;
  memset(&soon, 0, sizeof soon);
  soon.it_value.tv_nsec = 10 * 1000 * 1000;
  timer_settime(timer, 0, &soon, NULL);
}

static void* unblocked_sleeper(void* arg) {
  (void)arg;
  sigset_t rt_only;
  sigemptyset(&rt_only);
  sigaddset(&rt_only, SIGRTMIN + 1);
  pthread_sigmask(SIG_UNBLOCK, &rt_only, NULL);
  struct timespec long_sleep = {0, 200 * 1000 * 1000};
  nanosleep(&long_sleep, NULL);
  return NULL;
}

static void* blocked_sleeper(void* arg) {
  (void)arg;
  sigset_t rt_only;
  sigemptyset(&rt_only);
  sigaddset(&rt_only, SIGRTMIN + 1);
  pthread_sigmask(SIG_BLOCK, &rt_only, NULL);
  struct timespec long_sleep = {0, 100 * 1000 * 1000};
  nanosleep(&long_sleep, NULL);
  return NULL;
}

static void* yielder(void* arg) {
  (void)arg;
  for (int i = 0; i < 300; i++) {
    sched_yield();
  }
  return NULL;
}

static void* arm_and_exit(void* arg) {
  (void)arg;
  arm_soon(SIGRTMIN + 1);
  return NULL;
}

static void on_usr1(int signo) {
  (void)signo;
  /* sa_mask blocks every signal here, SIGRTMIN+1 among them. */
  arm_soon(SIGRTMIN + 1);
  sleep_past_the_deadline();
  printf("handler returning\n");
}

/* Cases whose expiration Linux delivers at once (or as the handler
 * returns). Returns 1 if argv[1] named one. */
static int delivered_at_once(const char* which) {
  int rt = SIGRTMIN + 1;
  sigset_t rt_only;
  sigemptyset(&rt_only);
  sigaddset(&rt_only, rt);
  if (strcmp(which, "unblocked-at-expiry") == 0) {
    arm_soon(rt);
    sleep_past_the_deadline();
  } else if (strcmp(which, "thread-unblocked") == 0) {
    sigprocmask(SIG_BLOCK, &rt_only, NULL);
    pthread_t thread;
    pthread_create(&thread, NULL, unblocked_sleeper, NULL);
    struct timespec settle = {0, 1000 * 1000};
    nanosleep(&settle, NULL);
    arm_soon(rt);
    pthread_join(thread, NULL);
  } else if (strcmp(which, "signalfd-before") == 0) {
    /* The descriptor exists before the expiry, so reading it takes the
     * signal. */
    sigprocmask(SIG_BLOCK, &rt_only, NULL);
    int fd = signalfd(-1, &rt_only, SFD_NONBLOCK);
    arm_soon(rt);
    sleep_past_the_deadline();
    struct signalfd_siginfo info;
    printf("read %zd\n", read(fd, &info, sizeof info));
  } else if (strcmp(which, "pselect-big") == 0) {
    /* The call sleeps outside the schedule under a temporary mask that
     * blocks the signal; the thread's own mask, restored as it returns, does
     * not. */
    arm_soon(rt);
    struct timespec wait = {0, 50 * 1000 * 1000};
    pselect(101, NULL, NULL, NULL, &wait, &rt_only);
  } else if (strcmp(which, "arm-exit-thread") == 0) {
    /* A thread arms a process-directed timer and exits; the leader, which
     * does not block the signal, gets it. */
    pthread_t thread;
    pthread_create(&thread, NULL, arm_and_exit, NULL);
    pthread_join(thread, NULL);
    struct timespec wait = {0, 100 * 1000 * 1000};
    nanosleep(&wait, NULL);
  } else if (strcmp(which, "pselect-big-blocked-peer") == 0) {
    /* As pselect-big, with a second thread, in the schedule, that blocks the
     * signal: only the leader's restored mask lets it through. */
    pthread_t thread;
    pthread_create(&thread, NULL, blocked_sleeper, NULL);
    struct timespec settle = {0, 1000 * 1000};
    nanosleep(&settle, NULL);
    arm_soon(rt);
    struct timespec wait = {0, 50 * 1000 * 1000};
    pselect(101, NULL, NULL, NULL, &wait, &rt_only);
    pthread_join(thread, NULL);
  } else if (strcmp(which, "pselect-race") == 0) {
    /* A thread that keeps the schedule busy while the leader's pselect sleeps
     * outside it under a temporary mask that blocks the signal, returning
     * close to the expiry; the leader's own mask does not block it. */
    pthread_t thread;
    pthread_create(&thread, NULL, yielder, NULL);
    arm_soon(rt);
    struct timespec wait = {0, 5200 * 1000};
    pselect(101, NULL, NULL, NULL, &wait, &rt_only);
    sleep_past_the_deadline();
    pthread_join(thread, NULL);
  } else if (strcmp(which, "handler-sigreturn") == 0) {
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = on_usr1;
    sigfillset(&action.sa_mask);
    sigaction(SIGUSR1, &action, NULL);
    raise(SIGUSR1);
    printf("after handler\n");
  } else {
    return 0;
  }
  printf("survived\n");
  return 1;
}

int main(int argc, char** argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: %s CASE\n", argv[0]);
    return 2;
  }
  setvbuf(stdout, NULL, _IONBF, 0);
  const char* which = argv[1];
  if (delivered_at_once(which)) {
    return 0;
  }
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
