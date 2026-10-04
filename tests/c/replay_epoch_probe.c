/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Guest for the record/replay virtual-time epoch tests
// (https://github.com/rrnewton/hermit/issues/3411).
//
// Two threads spin without syscalls long enough to cross several preemption
// timeslices, so a `--chaos --record-preemptions-to` recording on a host with
// a PMU carries per-thread preemption points. Those points are absolute
// logical times, i.e. offsets from the run's virtual-time epoch
// (run_chaos_preemption_replay_reuses_the_recorded_epoch in
// hermit-cli/tests/cli.rs). The main thread also samples CLOCK_REALTIME
// between spins, so stdout carries the epoch and the clock's progress; the
// PMU-free epoch adoption and refusal cases in
// hermit-cli/tests/clock_determinism.rs compare those samples between a
// recording and its replay.

#include <inttypes.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>

enum { SAMPLES = 8 };
static const uint64_t ITERATIONS = 40000000;

static volatile uint64_t sink;

static void spin(uint64_t iterations) {
  for (uint64_t i = 0; i < iterations; i++) {
    sink += i * UINT64_C(2654435761);
  }
}

static void* worker(void* arg) {
  (void)arg;
  spin(ITERATIONS);
  return NULL;
}

int main(void) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  for (int i = 0; i < SAMPLES; i++) {
    spin(ITERATIONS / SAMPLES);
    struct timespec now;
    if (clock_gettime(CLOCK_REALTIME, &now) != 0) {
      perror("clock_gettime");
      return 1;
    }
    printf("sample %d %lld.%09ld\n", i, (long long)now.tv_sec, now.tv_nsec);
  }
  if (pthread_join(thread, NULL) != 0) {
    perror("pthread_join");
    return 1;
  }
  puts("replay-epoch-probe-ok");
  return 0;
}
