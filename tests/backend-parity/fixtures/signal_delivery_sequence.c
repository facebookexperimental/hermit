/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Signal handling sequence: sigaction, blocking, pending inspection, delivery.
 *
 * Installs a counting SIGUSR1 handler, blocks SIGUSR1 via sigprocmask, raises
 * it N times while blocked (so the kernel coalesces them into ONE pending
 * instance), asserts it shows in sigpending, then unblocks and observes exactly
 * one delivery. Finally it raises N times UNBLOCKED and observes N deliveries.
 *
 * This is the standard-but-easily-broken POSIX contract that non-realtime
 * signals do not queue: a backend that replays or re-injects blocked signals
 * naively reports N deliveries in the coalesced phase instead of 1.
 *
 * Deterministic by construction: the observables are two delivery counts and a
 * pending-bit, all fixed by the standard. No pid, timestamp, or ordering.
 */

/* The e2e harness compiles with -std=c11, which hides POSIX declarations. */
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define RAISES 5

static volatile sig_atomic_t delivered = 0;

static void handler(int signo) {
  (void)signo;
  delivered++;
}

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/*
 * The e2e harness has no golden-output field: its verify oracle is exit status
 * plus cross-attempt determinism. A deterministically wrong stdout therefore
 * passes unnoticed unless the guest checks itself, so every invariant below is
 * asserted rather than merely printed.
 */
static int violations;

static void expect(const char *name, long long observed, long long wanted) {
  if (observed != wanted) {
    fprintf(stderr, "invariant %s: observed %lld, wanted %lld\n", name, observed,
            wanted);
    violations++;
  }
}

int main(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = handler;
  sigemptyset(&sa.sa_mask);
  if (sigaction(SIGUSR1, &sa, NULL) != 0)
    fail("sigaction");

  sigset_t block, previous;
  sigemptyset(&block);
  sigaddset(&block, SIGUSR1);
  if (sigprocmask(SIG_BLOCK, &block, &previous) != 0)
    fail("sigprocmask block");

  for (int i = 0; i < RAISES; ++i)
    if (raise(SIGUSR1) != 0)
      fail("raise while blocked");

  sigset_t pending;
  sigemptyset(&pending);
  if (sigpending(&pending) != 0)
    fail("sigpending");
  int was_pending = sigismember(&pending, SIGUSR1) == 1 ? 1 : 0;

  if (sigprocmask(SIG_SETMASK, &previous, NULL) != 0)
    fail("sigprocmask restore");
  int coalesced = (int)delivered;

  delivered = 0;
  for (int i = 0; i < RAISES; ++i)
    if (raise(SIGUSR1) != 0)
      fail("raise while unblocked");
  int direct = (int)delivered;

  expect("raised", (long long)RAISES, 5);
  expect("pending", (long long)was_pending, 1);
  expect("coalesced", (long long)coalesced, 1);
  expect("direct", (long long)direct, 5);
  printf("signals raised=%d pending=%d coalesced=%d direct=%d\n", RAISES,
         was_pending, coalesced, direct);
  return violations == 0 ? 0 : 1;
}
