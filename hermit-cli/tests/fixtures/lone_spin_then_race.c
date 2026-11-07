/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A main thread that computes alone, then races a worker.
//
// Modes, the first argument:
// - "wait": the worker waits on a condition variable (a futex wait without a
//   timeout, so no other thread can run before main's next syscall) while
//   main runs a 25000-iteration loop without a syscall; then both add 5000 to
//   a shared counter without a lock.
// - "wait-short": "wait" with an 8000-iteration loop, for the replay test
//   that runs this guest eight times; at 25000 iterations its counted CPU
//   cost outgrew the per-test budget under load, and 8000 still records
//   hundreds of stopless slice ends and timer checkpoints.
// - "sleep": the worker naps 100 us at a time (a timed wait, so any slice end
//   may hand it the turn) through main's loop, of 8000 iterations here, and
//   main exits without waking it.
// - "rdtsc": "wait", with an rdtsc every 1000 iterations of main's loop (an
//   instruction Hermit traps).
// - "cpuid": "wait", with a cpuid every 1000 iterations of main's loop (an
//   instruction Hermit traps through CPUID faulting).
// - "segv": "wait", with a null store right after main's loop that a SIGSEGV
//   handler, installed before the loop, recovers from; the fault is the
//   first event after the loop.
// - "long": "wait" with 1250000 iterations, more branches than the backstop
//   timer of a thread running alone waits (2000000): it fires once, at about
//   2020000.
// - "exec": no worker; main runs the loop alone, then execs this program with
//   "done", which prints and exits.
// Prints the loop's sum, the counter and CLOCK_MONOTONIC in nanoseconds.

#include <cpuid.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <x86intrin.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t go = PTHREAD_COND_INITIALIZER;
static int started;
static int napping;
static volatile long counter;
static sigjmp_buf recover;
static volatile unsigned long ticks;

static void add(void) {
  for (int i = 0; i < 5000; i++) {
    counter = counter + 1;
  }
}

static void* worker(void* arg) {
  (void)arg;
  pthread_mutex_lock(&lock);
  while (!started) {
    if (napping) {
      pthread_mutex_unlock(&lock);
      struct timespec nap = {0, 100000};
      nanosleep(&nap, NULL);
      pthread_mutex_lock(&lock);
    } else {
      pthread_cond_wait(&go, &lock);
    }
  }
  pthread_mutex_unlock(&lock);
  add();
  return NULL;
}

// The loop's result, kept outside main's frame so that it survives the
// handler's siglongjmp.
static volatile unsigned long loop_sum;

static void on_segv(int signal) {
  (void)signal;
  siglongjmp(recover, 1);
}

// What main's loop traps every 1000 iterations.
enum trap { TRAP_NONE, TRAP_RDTSC, TRAP_CPUID };

static unsigned long compute(unsigned long iterations, enum trap trap) {
  volatile unsigned long sum = 0;
  for (unsigned long i = 0; i < iterations; i++) {
    sum += i;
    if (trap != TRAP_NONE && i % 1000 == 0) {
      if (trap == TRAP_RDTSC) {
        ticks += __rdtsc();
      } else {
        unsigned int a, b, c, d;
        __cpuid(0, a, b, c, d);
        ticks += a ^ b ^ c ^ d;
      }
    }
  }
  return sum;
}

static void print(unsigned long sum) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  printf(
      "sum=%lu counter=%ld monotonic_ns=%lld\n",
      sum,
      counter,
      (long long)now.tv_sec * 1000000000LL + now.tv_nsec);
  fflush(stdout);
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "wait";
  if (strcmp(mode, "done") == 0) {
    print(0);
    return 0;
  }
  if (strcmp(mode, "exec") == 0) {
    print(compute(25000, TRAP_NONE));
    char* const args[] = {argv[0], "done", NULL};
    execv(argv[0], args);
    return 2;
  }
  napping = strcmp(mode, "sleep") == 0;
  pthread_t thread;
  pthread_create(&thread, NULL, worker, NULL);
  unsigned long iterations = 25000;
  if (strcmp(mode, "long") == 0) {
    iterations = 1250000;
  } else if (napping || strcmp(mode, "wait-short") == 0) {
    iterations = 8000;
  }
  int segv = strcmp(mode, "segv") == 0;
  if (segv) {
    signal(SIGSEGV, on_segv);
  }
  if (!segv || sigsetjmp(recover, 1) == 0) {
    enum trap trap = TRAP_NONE;
    if (strcmp(mode, "rdtsc") == 0) {
      trap = TRAP_RDTSC;
    } else if (strcmp(mode, "cpuid") == 0) {
      trap = TRAP_CPUID;
    }
    loop_sum = compute(iterations, trap);
    if (segv) {
      *(volatile int*)0 = 1;
    }
  }
  unsigned long sum = loop_sum;
  if (napping) {
    print(sum);
    _exit(0);
  }
  pthread_mutex_lock(&lock);
  started = 1;
  pthread_cond_signal(&go);
  pthread_mutex_unlock(&lock);
  add();
  pthread_join(thread, NULL);
  print(sum);
  return 0;
}
