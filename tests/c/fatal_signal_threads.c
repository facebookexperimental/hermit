/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A forked child raises SIGTERM, and its parent reports how the child ended.
 * No robust mutex is involved (https://github.com/rrnewton/hermit/issues/4278).
 *
 * - single-default: the child has one thread and SIGTERM's default action,
 *   which ends it.
 * - threaded-default: the same, with a second thread blocked in pause().
 * - threaded-caught: the same two threads, with a SIGTERM handler installed:
 *   the handler runs and the child exits normally.
 * - threaded-race: the second thread yields twice, installs a SIGTERM handler
 *   and then stores 1 to a page it shares with the parent, while the first
 *   raises SIGTERM. The parent also prints that word. Linux commits a default
 *   action under the signal lock and kills every other thread at once, so
 *   "signaled sig=15 shm=1" never happens: either the handler runs (exited),
 *   or the sibling never runs again after the decision (shm=0). Under
 *   Hermit's DBT schedule the sibling's turn falls right after the decision
 *   (Codex review and Claude re-check of
 *   https://github.com/rrnewton/hermit/pull/4387).
 */

#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static _Atomic int sleeper_started;
static _Atomic int caught;
static volatile int* shared;

static void on_sigterm(int signal) {
  (void)signal;
  atomic_store(&caught, 1);
}

static void* late_handler(void* unused) {
  (void)unused;
  sched_yield();
  sched_yield();
  signal(SIGTERM, on_sigterm);
  shared[0] = 1;
  for (;;) {
    pause();
  }
  return NULL;
}

static void* sleeper(void* unused) {
  (void)unused;
  atomic_store(&sleeper_started, 1);
  for (;;) {
    pause();
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(
        stderr,
        "usage: %s single-default|threaded-default|threaded-caught|threaded-race\n",
        argv[0]);
    return 64;
  }
  const char* mode = argv[1];
  int race = strcmp(mode, "threaded-race") == 0;
  int threaded = strcmp(mode, "single-default") != 0;
  int handled = strcmp(mode, "threaded-caught") == 0;
  if (!threaded && !handled && strcmp(mode, "single-default") != 0) {
    return 64;
  }
  if (threaded && !handled && !race && strcmp(mode, "threaded-default") != 0) {
    return 64;
  }
  shared = mmap(
      NULL,
      4096,
      PROT_READ | PROT_WRITE,
      MAP_SHARED | MAP_ANONYMOUS,
      -1,
      0);
  if (shared == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    if (handled) {
      signal(SIGTERM, on_sigterm);
    }
    if (race) {
      pthread_t thread;
      if (pthread_create(&thread, NULL, late_handler, NULL) != 0) {
        _exit(2);
      }
    } else if (threaded) {
      pthread_t thread;
      if (pthread_create(&thread, NULL, sleeper, NULL) != 0) {
        _exit(2);
      }
      while (!atomic_load(&sleeper_started)) {
        sched_yield();
      }
    }
    raise(SIGTERM);
    _exit(atomic_load(&caught) ? 0 : 9);
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  char shm[16] = "";
  if (race) {
    snprintf(shm, sizeof shm, " shm=%d", shared[0]);
  }
  if (WIFSIGNALED(status)) {
    printf("FATAL-SIGNAL %s: signaled sig=%d%s\n", mode, WTERMSIG(status), shm);
  } else {
    printf("FATAL-SIGNAL %s: exited code=%d%s\n", mode, WEXITSTATUS(status), shm);
  }
  return 0;
}
