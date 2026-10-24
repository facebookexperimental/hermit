/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A parent's SIGCHLD for a child that ends, while a sibling thread stays
 * runnable until the parent has reaped the child.
 *
 * Usage: sigchld_busy_sibling <kill|exit|sigsuspend>
 *
 * kill:       the child waits in pause(); the parent SIGKILLs it and reaps
 *             it.
 * exit:       the child exits at once; the parent reaps it.
 * sigsuspend: the child exits at once; the parent, which blocks SIGCHLD
 *             from before the fork, waits in sigsuspend() until its SIGCHLD
 *             handler has run, then reaps it. (A pause() loop would race:
 *             a SIGCHLD handled between its check and the call leaves it
 *             waiting forever, on Linux too.)
 *
 * The sibling blocks SIGCHLD and calls sched_yield() until the parent sets
 * `done`, so it never blocks. On Linux the parent runs beside it and the
 * program prints one line and exits 0.
 */

#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static atomic_int done;
static atomic_int sibling_started;
static atomic_int handled;
static atomic_long yields;

static void on_sigchld(int sig) {
  (void)sig;
  atomic_store(&handled, 1);
}

static void* sibling(void* arg) {
  (void)arg;
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGCHLD);
  pthread_sigmask(SIG_BLOCK, &set, NULL);
  atomic_store(&sibling_started, 1);
  while (!atomic_load(&done)) {
    sched_yield();
    atomic_fetch_add(&yields, 1);
  }
  return NULL;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  int kill_mode = strcmp(mode, "kill") == 0;
  int suspend_mode = strcmp(mode, "sigsuspend") == 0;
  if (!kill_mode && !suspend_mode && strcmp(mode, "exit") != 0) {
    fprintf(stderr, "usage: %s <kill|exit|sigsuspend>\n", argv[0]);
    return 2;
  }
  sigset_t unblocked;
  if (suspend_mode) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_sigchld;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGCHLD, &sa, NULL) != 0) {
      perror("sigaction");
      return 2;
    }
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &chld, &unblocked) != 0) {
      perror("sigprocmask");
      return 2;
    }
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, sibling, NULL) != 0) {
    perror("pthread_create");
    return 2;
  }
  while (!atomic_load(&sibling_started)) {
    sched_yield();
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 2;
  }
  if (child == 0) {
    if (kill_mode) {
      for (;;) {
        pause();
      }
    }
    _exit(0);
  }
  if (kill_mode && kill(child, SIGKILL) != 0) {
    perror("kill");
    return 2;
  }
  if (suspend_mode) {
    while (!atomic_load(&handled)) {
      sigsuspend(&unblocked);
    }
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 2;
  }
  atomic_store(&done, 1);
  pthread_join(thread, NULL);
  printf("mode=%s exited=%d code=%d signaled=%d sig=%d handled=%d\n", mode,
         WIFEXITED(status), WIFEXITED(status) ? WEXITSTATUS(status) : 0,
         WIFSIGNALED(status), WIFSIGNALED(status) ? WTERMSIG(status) : 0,
         atomic_load(&handled));
  return 0;
}
