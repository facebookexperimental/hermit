/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A timed wait raced against a worker's progress, for
 * --chaos-timed-wait-spread.
 *
 * usage: timed_wait_spread <sleep|condwait>
 *
 * A worker takes 40 turns of a little work and a sched_yield, then sets a
 * flag. Meanwhile the main thread waits 50 ms: with nanosleep ("sleep"), or
 * with a pthread_cond_timedwait on the flag ("condwait"). It prints which
 * came first and how long the wait took on its own CLOCK_MONOTONIC:
 *
 *   first=worker waited_ns=N     the worker finished before the wait ended
 *   first=sleeper waited_ns=N    the sleep ended before the worker finished
 *   first=timeout waited_ns=N    the timed wait expired before the worker
 *
 * A sleep, and a timed wait that expired, must have lasted at least 50 ms on
 * the guest's clock, as on Linux; a shorter one prints SHORT and exits 1.
 */

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#define WAIT_NS 50000000LL
#define TURNS 40

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv;
static int done;
static volatile uint64_t sink;

static int64_t now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static void *worker(void *arg) {
  (void)arg;
  uint64_t x = 1;
  for (int turn = 0; turn < TURNS; turn++) {
    for (int i = 0; i < 1000; i++) {
      x = x * 6364136223846793005ULL + 1442695040888963407ULL;
    }
    sink += x;
    sched_yield();
  }
  pthread_mutex_lock(&mu);
  done = 1;
  pthread_cond_signal(&cv);
  pthread_mutex_unlock(&mu);
  return NULL;
}

static int read_done(void) {
  pthread_mutex_lock(&mu);
  int value = done;
  pthread_mutex_unlock(&mu);
  return value;
}

int main(int argc, char **argv) {
  if (argc != 2 || (strcmp(argv[1], "sleep") && strcmp(argv[1], "condwait"))) {
    fprintf(stderr, "usage: %s <sleep|condwait>\n", argv[0]);
    return 2;
  }
  pthread_condattr_t attr;
  pthread_condattr_init(&attr);
  pthread_condattr_setclock(&attr, CLOCK_MONOTONIC);
  pthread_cond_init(&cv, &attr);
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL)) {
    perror("pthread_create");
    return 2;
  }
  int64_t start = now_ns();
  const char *first;
  int must_be_full;
  if (!strcmp(argv[1], "sleep")) {
    struct timespec rest = {0, WAIT_NS};
    while (nanosleep(&rest, &rest) && errno == EINTR) {
    }
    first = read_done() ? "worker" : "sleeper";
    must_be_full = 1;
  } else {
    struct timespec deadline;
    clock_gettime(CLOCK_MONOTONIC, &deadline);
    deadline.tv_nsec += WAIT_NS;
    if (deadline.tv_nsec >= 1000000000) {
      deadline.tv_sec++;
      deadline.tv_nsec -= 1000000000;
    }
    int timed_out = 0;
    pthread_mutex_lock(&mu);
    while (!done && !timed_out) {
      timed_out = pthread_cond_timedwait(&cv, &mu, &deadline) == ETIMEDOUT && !done;
    }
    pthread_mutex_unlock(&mu);
    first = timed_out ? "timeout" : "worker";
    must_be_full = timed_out;
  }
  int64_t waited = now_ns() - start;
  pthread_join(thread, NULL);
  printf("first=%s waited_ns=%lld\n", first, (long long)waited);
  if (must_be_full && waited < WAIT_NS) {
    printf("SHORT: the wait ended %lld ns before its 50 ms\n", (long long)(WAIT_NS - waited));
    return 1;
  }
  return 0;
}
