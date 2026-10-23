/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A worker thread does finite work, one syscall per step, while main waits in
// sigsuspend for the SIGCHLD of a child process, then prints how far the
// worker had got when main woke. Under `--chaos --chaos-target-races` the
// worker can sit in the last priority band. When the band alone counted as a
// poller, main's host-observed wake was admitted between the worker's turns at
// a moment the host chose, so the progress printed varied from run to run
// (https://github.com/rrnewton/hermit/issues/4234).
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

static atomic_long progress;

static void on_sigchld(int signo) {
  (void)signo;
}

static void* work(void* unused) {
  (void)unused;
  for (long i = 0; i < 400; i++) {
    atomic_store(&progress, i);
    getppid(); // One syscall per step: a scheduler turn.
  }
  return NULL;
}

int main(void) {
  sigset_t blocked;
  sigemptyset(&blocked);
  sigaddset(&blocked, SIGCHLD);
  signal(SIGCHLD, on_sigchld);
  sigprocmask(SIG_BLOCK, &blocked, NULL);
  pthread_t worker;
  if (pthread_create(&worker, NULL, work, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  pid_t child = fork();
  if (child == 0) {
    for (int i = 0; i < 20; i++) {
      getpid();
    }
    _exit(0);
  }
  sigset_t none;
  sigemptyset(&none);
  sigsuspend(&none);
  long seen = atomic_load(&progress);
  printf("main woke, worker progress %ld\n", seen);
  pthread_join(worker, NULL);
  waitpid(child, NULL, 0);
  printf("done\n");
  return 0;
}
