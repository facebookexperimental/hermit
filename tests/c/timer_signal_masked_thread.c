/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A timer's process-directed signal must reach a thread that does not block
// it (https://github.com/rrnewton/hermit/issues/4320).
//
// (default) Main arms alarm(10) and waits in pause(). A worker blocks SIGALRM
//   and spins on sched_yield until main has woken. Under --chaos-target-races,
//   Detcore drew the worker as the target and woke it instead, and main slept
//   forever.
// armer-blocks: main blocks SIGALRM and arms alarm(1); a worker that does not
//   block it waits in pause(). Detcore named the arming thread, so the worker
//   was never woken, even without chaos.
// posix-armer-blocks: as armer-blocks, with a SIGEV_SIGNAL POSIX timer
//   delivering SIGUSR1.
// sleeper: main arms alarm(1) and waits in pause(); a worker waits in
//   sigsuspend with nothing blocked. Either may take SIGALRM on Linux; under
//   Hermit the choice must be the same in every run (Claude review of
//   https://github.com/rrnewton/hermit/pull/4331), and the handler must end
//   the worker's sigsuspend under SA_RESTART too. Prints which one took it.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static atomic_int woke;
static atomic_int taken_by_worker;
// sleeper: 1 if main took the first SIGALRM, 2 if the worker did.
static atomic_int first_taker;
static pthread_t worker_thread;
static pthread_t main_thread;

static void on_signal(int signo) {
  int expected = 0;
  int worker = pthread_equal(pthread_self(), worker_thread);
  if (worker) {
    atomic_store(&taken_by_worker, 1);
  }
  if (signo == SIGALRM) {
    atomic_compare_exchange_strong(&first_taker, &expected, worker ? 2 : 1);
  }
}

static void* spin(void* unused) {
  (void)unused;
  sigset_t blocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGALRM);
  pthread_sigmask(SIG_BLOCK, &blocked, NULL);
  while (!atomic_load(&woke)) {
    sched_yield();
  }
  return NULL;
}

static void* wait_in_pause(void* signo) {
  sigset_t open;
  sigemptyset(&open);
  sigaddset(&open, *(int*)signo);
  pthread_sigmask(SIG_UNBLOCK, &open, NULL);
  pause();
  return NULL;
}

static void* wait_in_sigsuspend(void* unused) {
  (void)unused;
  sigset_t none;
  sigemptyset(&none);
  sigsuspend(&none);
  // The worker took the timer's signal: main is still in pause.
  if (atomic_load(&first_taker) == 2) {
    pthread_kill(main_thread, SIGUSR2);
  }
  return NULL;
}

static void arm_posix_timer(int signo) {
  struct sigevent event;
  memset(&event, 0, sizeof(event));
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = signo;
  timer_t timer;
  struct itimerspec once = {{0, 0}, {1, 0}};
  if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0 ||
      timer_settime(timer, 0, &once, NULL) != 0) {
    perror("timer");
    _exit(1);
  }
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  if (!strcmp(mode, "armer-blocks") || !strcmp(mode, "posix-armer-blocks")) {
    int posix = !strcmp(mode, "posix-armer-blocks");
    static int signo;
    signo = posix ? SIGUSR1 : SIGALRM;
    signal(signo, on_signal);
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, signo);
    pthread_sigmask(SIG_BLOCK, &blocked, NULL);
    pthread_create(&worker_thread, NULL, wait_in_pause, &signo);
    if (posix) {
      arm_posix_timer(signo);
    } else {
      alarm(1);
    }
    pthread_join(worker_thread, NULL);
    printf("%s: the worker woke, taken by worker %d\n", mode, atomic_load(&taken_by_worker));
    return 0;
  }
  if (!strcmp(mode, "sleeper")) {
    main_thread = pthread_self();
    // signal() sets SA_RESTART, which must not restart sigsuspend: when the
    // host call reported ERESTARTSYS, the worker slept again and main never
    // woke.
    signal(SIGALRM, on_signal);
    signal(SIGUSR2, on_signal);
    pthread_create(&worker_thread, NULL, wait_in_sigsuspend, NULL);
    alarm(1);
    pause();
    int first = atomic_load(&first_taker);
    // Main took it: the worker is still in sigsuspend.
    if (first == 1) {
      pthread_kill(worker_thread, SIGUSR2);
    }
    pthread_join(worker_thread, NULL);
    printf("sleeper: SIGALRM taken by %s\n", first == 2 ? "worker" : "main");
    return 0;
  }
  signal(SIGALRM, on_signal);
  pthread_t worker;
  if (pthread_create(&worker, NULL, spin, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  alarm(10);
  pause();
  atomic_store(&woke, 1);
  pthread_join(worker, NULL);
  printf("main woke from pause\n");
  return 0;
}
