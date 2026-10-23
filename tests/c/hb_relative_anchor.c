/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Two threads for hermit-cli/tests/cli.rs's relative happens-before anchor
 * tests. The main thread (A) writes "a1" and "a2", makes three getppid calls,
 * then writes "A-after". A pthread worker (B) writes "B".
 *
 * - "order" (the default): B first makes 200 sched_yield calls, so in the
 *   natural schedule A finishes before B writes. A spec that holds A at "the
 *   first getppid after its second write" until B's write reverses that.
 * - "spin": B spins, with no syscalls, until A has passed its first getppid
 *   after "a2", then writes. Holding A there until B's write can never be
 *   satisfied, and B never ends its turn by itself; only the preemption timer
 *   lets virtual time run, so the hold budget ends the run.
 * - "late": as "order", but A writes "a1" and "a2" before it creates B, so
 *   anything B inherited from A would already include A's markers.
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static atomic_int a_passed;
static int spin;
static int late;

static void say(const char *line) {
  size_t length = strlen(line);
  if (write(1, line, length) != (ssize_t)length) {
    _exit(2);
  }
}

static void *worker(void *arg) {
  (void)arg;
  if (spin) {
    while (!atomic_load_explicit(&a_passed, memory_order_relaxed)) {
    }
  } else {
    for (int i = 0; i < 200; i++) {
      sched_yield();
    }
  }
  say("B\n");
  return 0;
}

int main(int argc, char **argv) {
  spin = argc > 1 && strcmp(argv[1], "spin") == 0;
  late = argc > 1 && strcmp(argv[1], "late") == 0;
  if (late) {
    say("a1\n");
    say("a2\n");
  }
  pthread_t thread;
  if (pthread_create(&thread, 0, worker, 0) != 0) {
    return 3;
  }
  if (!late) {
    say("a1\n");
    say("a2\n");
  }
  for (int i = 0; i < 3; i++) {
    syscall(SYS_getppid);
    atomic_store_explicit(&a_passed, 1, memory_order_relaxed);
  }
  say("A-after\n");
  return pthread_join(thread, 0) == 0 ? 0 : 4;
}
