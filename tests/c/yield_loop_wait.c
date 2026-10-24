/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A worker thread spins on sched_yield while main waits in sigsuspend
// (https://github.com/rrnewton/hermit/issues/4234). Prints the worker's spin
// count, which must be the same in every run with the same seed.
//
// `wait`: the worker spins until main has woken. Main blocks SIGCHLD, forks a
// child that exits at once, and sigsuspends. Under --chaos-target-races the
// child-exit SIGCHLD can be sent while every thread still blocks it, so it
// waits in the shared queue and ends main's sigsuspend as soon as it begins.
// Nothing fixed that wake to a turn, so run mode held it behind the yielder
// forever.
// `timer`: as `wait`, beside a 1 ms periodic timer whose SIGUSR2 every thread
// blocks.
// `wait-idle`: as `wait`, beside a thread waiting in sigsuspend for a signal
// nobody sends; main ends the process with exit.
// `signal`: the worker yields 20000 times and then signals main, so main's
// call can only end after the worker runs again.
// `alarm`: main arms alarm(1) and waits in sigsuspend for SIGALRM, which only
// virtual time can bring; the worker spins until main has woken.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <stdlib.h>
#include <unistd.h>

static atomic_int woke;
static atomic_long spins;
static pthread_t main_thread;

static void on_signal(int signo) {
  (void)signo;
}

static void* wait_for_main(void* unused) {
  (void)unused;
  while (!atomic_load(&woke)) {
    atomic_fetch_add(&spins, 1);
    sched_yield();
  }
  return NULL;
}

static void* yield_then_signal(void* unused) {
  (void)unused;
  for (int i = 0; i < 20000; i++) {
    sched_yield();
  }
  pthread_kill(main_thread, SIGUSR1);
  return NULL;
}

static void start_periodic_timer(void) {
  struct sigevent event;
  memset(&event, 0, sizeof(event));
  event.sigev_notify = SIGEV_SIGNAL;
  event.sigev_signo = SIGUSR2;
  timer_t timer;
  if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0) {
    perror("timer_create");
    _exit(1);
  }
  struct itimerspec period = {{0, 1000000}, {0, 1000000}};
  if (timer_settime(timer, 0, &period, NULL) != 0) {
    perror("timer_settime");
    _exit(1);
  }
}

static void* idle_signal_thread(void* unused) {
  (void)unused;
  sigset_t waits_for;
  sigfillset(&waits_for);
  sigdelset(&waits_for, SIGUSR2);
  sigsuspend(&waits_for);
  return NULL;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "wait";
  int signal_mode = strcmp(mode, "signal") == 0;
  int alarm_mode = strcmp(mode, "alarm") == 0;
  int timer_mode = strcmp(mode, "timer") == 0;
  int idle_mode = strcmp(mode, "wait-idle") == 0;
  int signo = signal_mode ? SIGUSR1 : alarm_mode ? SIGALRM : SIGCHLD;
  sigset_t blocked, previous;
  sigemptyset(&blocked);
  sigaddset(&blocked, signo);
  if (timer_mode) {
    sigaddset(&blocked, SIGUSR2);
  }
  signal(signo, on_signal);
  sigprocmask(SIG_BLOCK, &blocked, &previous);
  if (timer_mode) {
    // Keep SIGUSR2 blocked in sigsuspend too: only SIGCHLD may end it.
    sigaddset(&previous, SIGUSR2);
    start_periodic_timer();
  }
  main_thread = pthread_self();
  pthread_t worker;
  if (pthread_create(
          &worker, NULL, signal_mode ? yield_then_signal : wait_for_main, NULL) !=
      0) {
    perror("pthread_create");
    return 1;
  }
  if (idle_mode) {
    pthread_t idle;
    if (pthread_create(&idle, NULL, idle_signal_thread, NULL) != 0) {
      perror("pthread_create");
      return 1;
    }
  }
  pid_t child = -1;
  if (alarm_mode) {
    alarm(1);
  } else if (!signal_mode) {
    child = fork();
    if (child == 0) {
      _exit(0);
    }
  }
  sigsuspend(&previous);
  atomic_store(&woke, 1);
  pthread_join(worker, NULL);
  if (child > 0) {
    waitpid(child, NULL, 0);
  }
  printf("done, %ld spins\n", atomic_load(&spins));
  if (idle_mode) {
    exit(0);
  }
  return 0;
}
