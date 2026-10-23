/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Creates and joins short-lived threads, one at a time. pthread_join waits on
 * the exiting thread's CLONE_CHILD_CLEARTID word, which Linux stores 0 to and
 * wakes in the thread's exit, after walking the thread's robust list. Under
 * Hermit a joiner must see the same word at the same point in every run, and
 * everything Linux did before the store must be done when the join returns
 * (https://github.com/rrnewton/hermit/issues/4246).
 *
 * Modes (argv[1]):
 *   join          200 threads that exit at once; prints "JOINED: 200 threads".
 *   robust-owner  each thread exits holding a robust mutex; right after the
 *                 join, trylock must return EOWNERDEAD, because Linux marks
 *                 the mutex FUTEX_OWNER_DIED before it clears the TID.
 *   reused-stack  50 rounds: after joining thread A, thread B is created,
 *                 usually on A's cached stack; B's TID word must keep B's TID
 *                 while B lives, because nothing of A's exit may store to it
 *                 after the join. Fails unless B reused A's stack at least
 *                 once, so the check cannot pass vacuously.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define THREADS 200
/* Each round spins on sched_yield, a scheduled syscall under Hermit. */
#define REUSED_STACK_ROUNDS 50

static void* exit_at_once(void* argument) {
  return argument;
}

static int join_threads(void) {
  long joined = 0;
  for (long i = 0; i < THREADS; ++i) {
    pthread_t thread;
    void* result;
    if (pthread_create(&thread, NULL, exit_at_once, (void*)i) != 0 ||
        pthread_join(thread, &result) != 0 || (long)result != i) {
      fprintf(stderr, "thread %ld failed\n", i);
      return 1;
    }
    ++joined;
  }
  printf("JOINED: %ld threads\n", joined);
  return 0;
}

static pthread_mutex_t robust;

static void* exit_holding_the_mutex(void* argument) {
  int rc = pthread_mutex_lock(&robust);
  if (rc == EOWNERDEAD) {
    pthread_mutex_consistent(&robust);
  } else if (rc != 0) {
    fprintf(stderr, "owner lock: %s\n", strerror(rc));
  }
  return argument;
}

static int join_robust_owners(void) {
  pthread_mutexattr_t attributes;
  pthread_mutexattr_init(&attributes);
  pthread_mutexattr_setrobust(&attributes, PTHREAD_MUTEX_ROBUST);
  pthread_mutex_init(&robust, &attributes);
  long dead = 0;
  for (long i = 0; i < THREADS; ++i) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, exit_holding_the_mutex, NULL) != 0 ||
        pthread_join(thread, NULL) != 0) {
      fprintf(stderr, "thread %ld failed\n", i);
      return 1;
    }
    int rc = pthread_mutex_trylock(&robust);
    if (rc != EOWNERDEAD) {
      fprintf(
          stderr, "round %ld: trylock after join returned %s\n", i, strerror(rc));
      return 2;
    }
    ++dead;
    pthread_mutex_consistent(&robust);
    pthread_mutex_unlock(&robust);
  }
  printf("EOWNERDEAD AFTER JOIN: %ld of %d\n", dead, THREADS);
  return 0;
}

static volatile pid_t lingering_tid;
static volatile int lingering_may_exit;

static void* linger(void* argument) {
  lingering_tid = (pid_t)syscall(SYS_gettid);
  while (!lingering_may_exit) {
    syscall(SYS_sched_yield);
  }
  return argument;
}

static int join_onto_reused_stacks(void) {
  long tid_offset = -1, reused = 0;
  for (long i = 0; i < REUSED_STACK_ROUNDS; ++i) {
    pthread_t quick, lingering;
    if (pthread_create(&quick, NULL, exit_at_once, NULL) != 0 ||
        pthread_join(quick, NULL) != 0) {
      fprintf(stderr, "quick thread %ld failed\n", i);
      return 1;
    }
    lingering_tid = 0;
    lingering_may_exit = 0;
    if (pthread_create(&lingering, NULL, linger, NULL) != 0) {
      fprintf(stderr, "lingering thread %ld failed\n", i);
      return 1;
    }
    while (lingering_tid == 0) {
      syscall(SYS_sched_yield);
    }
    if (pthread_equal(lingering, quick)) {
      ++reused;
    }
    unsigned char* descriptor = (unsigned char*)lingering;
    if (tid_offset < 0) {
      for (long offset = 0; offset < 4096; offset += 4) {
        if (*(volatile pid_t*)(descriptor + offset) == lingering_tid) {
          tid_offset = offset;
          break;
        }
      }
      if (tid_offset < 0) {
        fprintf(stderr, "TID word not found\n");
        return 4;
      }
    }
    for (int k = 0; k < 20; ++k) {
      syscall(SYS_sched_yield);
      if (*(volatile pid_t*)(descriptor + tid_offset) != lingering_tid) {
        fprintf(stderr, "round %ld: a live thread's TID word was cleared\n", i);
        return 3;
      }
    }
    lingering_may_exit = 1;
    if (pthread_join(lingering, NULL) != 0) {
      fprintf(stderr, "lingering join %ld failed\n", i);
      return 1;
    }
  }
  /* The check is vacuous unless B landed on A's stack at least once. */
  if (reused == 0) {
    fprintf(stderr, "no thread was created on a joined thread's stack\n");
    return 5;
  }
  printf("LIVE TID WORDS KEPT: %d rounds\n", REUSED_STACK_ROUNDS);
  return 0;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "join";
  if (strcmp(mode, "join") == 0) {
    return join_threads();
  }
  if (strcmp(mode, "robust-owner") == 0) {
    return join_robust_owners();
  }
  if (strcmp(mode, "reused-stack") == 0) {
    return join_onto_reused_stacks();
  }
  fprintf(stderr, "unknown mode %s\n", mode);
  return 64;
}
