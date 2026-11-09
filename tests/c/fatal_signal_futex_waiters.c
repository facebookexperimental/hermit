/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The process Hermit starts ends by a signal's default action while its other
 * threads are all blocked in futex waits, as a watchdog thread's abort() ends
 * a program whose workers wait on a condition variable. Linux kills the whole
 * thread group at that delivery, and the program's status is the signal.
 *
 * Three waiters block on a condition variable no one signals, and the main
 * thread joins the first of them. Once every waiter has counted itself under
 * the mutex and released it in pthread_cond_wait, the last thread prints a
 * line and ends the process:
 *
 *   abort   abort(3): SIGABRT, a core-dumping default action.
 *   term    raise(3) of SIGTERM: a terminating default action.
 *   segv    a store through a null pointer: a SIGSEGV Linux forces.
 *
 * A second argument changes the shape:
 *
 *   forked    a child process does all of this, and the process Hermit
 *             starts reports how the child ended and exits 0, so that
 *             `--verify` can compare two runs of it.
 *   sleeping  once counted, each waiter sleeps in a loop of 1 ms nanosleeps
 *             instead of waiting on the condition variable, so the threads
 *             the delivery ends are in timed waits.
 *
 * Every line printed is fixed.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define WAITERS 3

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t never = PTHREAD_COND_INITIALIZER;
static pthread_cond_t counted = PTHREAD_COND_INITIALIZER;
static int waiting;
static int sleeping;

static void* waiter(void* unused) {
  (void)unused;
  pthread_mutex_lock(&lock);
  waiting++;
  pthread_cond_signal(&counted);
  if (sleeping) {
    pthread_mutex_unlock(&lock);
    for (;;) {
      usleep(1000);
    }
  }
  for (;;) {
    pthread_cond_wait(&never, &lock);
  }
  return NULL;
}

static void end_process(const char* mode) {
  if (strcmp(mode, "abort") == 0) {
    abort();
  } else if (strcmp(mode, "term") == 0) {
    raise(SIGTERM);
  } else if (strcmp(mode, "segv") == 0) {
    *(volatile int*)NULL = 1;
  }
}

static void* ender(void* mode) {
  pthread_mutex_lock(&lock);
  while (waiting < WAITERS) {
    pthread_cond_wait(&counted, &lock);
  }
  pthread_mutex_unlock(&lock);
  printf("%d waiters blocked, ending by %s\n", WAITERS, (const char*)mode);
  fflush(stdout);
  end_process(mode);
  return NULL;
}

static int run_threads(const char* mode) {
  pthread_t waiters[WAITERS];
  for (int i = 0; i < WAITERS; i++) {
    if (pthread_create(&waiters[i], NULL, waiter, NULL) != 0) {
      return 2;
    }
  }
  pthread_t last;
  if (pthread_create(&last, NULL, ender, (void*)mode) != 0) {
    return 2;
  }
  pthread_join(waiters[0], NULL);
  return 3;
}

static int usage(const char* program) {
  fprintf(stderr, "usage: %s abort|term|segv [forked|sleeping]\n", program);
  return 64;
}

int main(int argc, char** argv) {
  if (argc < 2 || argc > 3) {
    return usage(argv[0]);
  }
  const char* mode = argv[1];
  if (strcmp(mode, "abort") != 0 && strcmp(mode, "term") != 0 &&
      strcmp(mode, "segv") != 0) {
    return usage(argv[0]);
  }
  if (argc == 2) {
    return run_threads(mode);
  }
  if (strcmp(argv[2], "sleeping") == 0) {
    sleeping = 1;
    return run_threads(mode);
  }
  if (strcmp(argv[2], "forked") != 0) {
    return usage(argv[0]);
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    _exit(run_threads(mode));
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  if (WIFSIGNALED(status)) {
    printf("child ended by signal %d\n", WTERMSIG(status));
  } else {
    printf("child exited %d\n", WEXITSTATUS(status));
  }
  return 0;
}
