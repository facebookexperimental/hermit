/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A setter thread and a spinner thread that can only both finish if
// Hermit preempts a thread spinning in pure userspace
// (https://github.com/facebookexperimental/hermit/issues/119).
//
// The setter waits (pure spin, no syscalls) for the started flag,
// burns a fixed amount of arithmetic, then sets the done flag. The
// main thread sets started and spins in pure userspace -- no
// syscalls, exactly the gap chaos_spinner_and_new_thread.c leaves
// open by calling getpid in its loop -- until done is set.
// Whichever thread the scheduler runs first after clone, it then
// spins without yielding, so under a backend with no preemption
// timer the peer never runs again and a cap below is reached: a
// bounded FAIL (measured 2 s wall under ptrace with the timeslice
// disabled; allow ~4 s worst case), never a hang. Under a
// preempting backend the setter completes inside its slices and
// the guest passes. Observable exits are only 0 (pass) and the
// starvation codes the spinner returns (2 setter error, 3 spinner
// cap); the setter's own returns are relayed through setter_error,
// which the spinner observes on the same terms as done.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>

static atomic_int started;
static atomic_int done;
static atomic_int setter_error;
static atomic_ullong burn_sink;

enum {
  // Pure-spin caps: reached in well under the cell CPU budget when a
  // peer is starved, so starvation fails fast instead of hanging.
  SPIN_CAP = 2000000000LL,
  // The setter's burn: long enough that it cannot finish inside one
  // scheduling decision, short enough to finish in a few slices.
  BURN_STEPS = 20000000LL,
};

static void* setter(void* arg) {
  (void)arg;
  long long waited = 0;
  while (!atomic_load(&started)) {
    if (++waited >= SPIN_CAP) {
      atomic_store(&setter_error, 2);
      return NULL;
    }
  }
  // Fixed arithmetic burn. The per-iteration atomic read-modify-write
  // is an observable side effect the optimizer must keep at any -O
  // level (a plain sum loop could be closed-form collapsed); the
  // final value is still the closed-form sum and is checked.
  for (long long i = 1; i <= BURN_STEPS; i++) {
    atomic_fetch_add_explicit(&burn_sink, (unsigned long long)i,
                              memory_order_relaxed);
  }
  if (atomic_load(&burn_sink) !=
      (unsigned long long)BURN_STEPS * (BURN_STEPS + 1) / 2) {
    atomic_store(&setter_error, 4);
    return NULL;
  }
  atomic_store(&done, 1);
  return NULL;
}

int main(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, setter, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  atomic_store(&started, 1);
  long long spun = 0;
  int error = 0;
  while (!atomic_load(&done) && !(error = atomic_load(&setter_error))) {
    if (++spun >= SPIN_CAP) {
      printf("spinner starved: no preemption of a pure userspace loop\n");
      return 3;
    }
  }
  pthread_join(thread, NULL);
  if (error) {
    printf("setter failed with %d\n", error);
    return error;
  }
  printf("preempt_spin_flag: setter completed while spinner spun\n");
  return 0;
}
