/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A futex word changed under a parked waiter, then a process-directed SIGALRM
 * (https://github.com/rrnewton/hermit/issues/4033). Linux-legal outcomes:
 *   - the signal goes to main and the waiter is never woken: the final
 *     FUTEX_WAKE wakes it, 0 (the usual native outcome);
 *   - the waiter is woken for the signal but main takes it: the woken waiter
 *     keeps TIF_SIGPENDING, so __futex_wait returns -ERESTARTSYS, get_signal
 *     finds nothing to deliver, and the restarted FUTEX_WAIT finds the changed
 *     word, EAGAIN;
 *   - the handler runs on the waiter: the wait is interrupted and restarted
 *     (SA_RESTART), and the restarted FUTEX_WAIT finds the changed word,
 *     EAGAIN.
 * A waiter woken by the signal that answers 0 is not legal.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

static uint32_t word __attribute__((aligned(64)));
static atomic_int handler_tid;
static atomic_int waiter_tid;
static long wait_result;
static int wait_errno;

static void on_alrm(int sig) {
  (void)sig;
  atomic_store(&handler_tid, gettid());
}

static void* waiter(void* unused) {
  (void)unused;
  atomic_store(&waiter_tid, gettid());
  wait_result = syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
  wait_errno = errno;
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_alrm;
  action.sa_flags = SA_RESTART;
  sigaction(SIGALRM, &action, NULL);
  pthread_t thread;
  pthread_create(&thread, NULL, waiter, NULL);
  while (atomic_load(&waiter_tid) == 0) {
    sched_yield();
  }
  for (int i = 0; i < 1000; i++) {
    sched_yield();
  }
  /* The word changes while the waiter is parked, with no wake. */
  word = 1;
  struct itimerval timer = {{0, 0}, {0, 2000}};
  setitimer(ITIMER_REAL, &timer, NULL);
  while (atomic_load(&handler_tid) == 0) {
    sched_yield();
  }
  for (int i = 0; i < 1000; i++) {
    sched_yield();
  }
  syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
  pthread_join(thread, NULL);
  int who = atomic_load(&handler_tid);
  printf("handler on %s, waiter %s\n", who == gettid() ? "main" : "waiter",
         wait_result == 0 ? "woken (0)" : strerrorname_np(wait_errno));
  return 0;
}
