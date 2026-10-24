/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A thread that returns from a signal handler and then runs without a
 * syscall must still be preempted (A5 step 2c,
 * https://github.com/rrnewton/hermit/issues/4485).
 *
 * The main thread signals itself K times. Iteration i's handler retires
 * about BASE + i * STEP conditional branches and returns; the main thread
 * then spins, with no syscall, until the worker acknowledges iteration i.
 * The worker spins without syscalls too, so under Hermit's sequentialized
 * scheduler each side runs only once the PMU preemption timer preempts the
 * other. With Detcore's signal-state copy on, ptrace reports each handler
 * return, and before step 2c a return within Reverie's keep margin (10005
 * branches) of the slice's timer target cancelled the event for good: the
 * main thread spun for ever. The cell's --max-timeslice=100000 (10,000
 * branches) is shorter than the margin, so the first return already lands
 * within it of whatever slice target is pending, and one iteration is
 * enough. More iterations add PMU interrupts, each a chance of a host-timed
 * skid overshoot, and each slice end single-steps the last skid margin of
 * branches, which a loaded host makes slow.
 */
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#define K 1
#define BASE 1000L
#define STEP 4001L

/* Lock-free, so the handler may use them, and free of data races between
 * the two threads. */
_Static_assert(ATOMIC_LONG_LOCK_FREE == 2, "atomic long must be lock-free");
_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "atomic int must be lock-free");
static atomic_long req = -1;
static atomic_long ack = -1;
static atomic_long spin_for;
static atomic_int handled;

static void handler(int sig) {
  (void)sig;
  long n = atomic_load(&spin_for);
  for (volatile long j = 0; j < n; j++)
    ;
  atomic_fetch_add(&handled, 1);
}

static void* worker(void* arg) {
  (void)arg;
  while (atomic_load(&ack) < K - 1) {
    long r = atomic_load(&req);
    if (r > atomic_load(&ack))
      atomic_store(&ack, r);
  }
  return NULL;
}

int main(void) {
  struct sigaction sa = {0};
  sa.sa_handler = handler;
  if (sigaction(SIGUSR1, &sa, NULL) != 0)
    return 2;
  pthread_t t;
  if (pthread_create(&t, NULL, worker, NULL) != 0)
    return 3;
  pid_t pid = getpid();
  pid_t tid = (pid_t)syscall(SYS_gettid);
  for (long i = 0; i < K; i++) {
    atomic_store(&spin_for, BASE + i * STEP);
    if (syscall(SYS_tgkill, pid, tid, SIGUSR1) != 0)
      return 4;
    atomic_store(&req, i);
    while (atomic_load(&ack) < i)
      ;
  }
  if (pthread_join(t, NULL) != 0)
    return 5;
  /* The handler ran once per signal, before the signal's tgkill returned,
   * so the run exercised the handler returns it is meant to. */
  int ran = atomic_load(&handled);
  printf("done %d iterations, handler ran %d times\n", K, ran);
  return ran == K ? 0 : 6;
}
