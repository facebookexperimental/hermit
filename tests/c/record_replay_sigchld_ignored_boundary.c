/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* A child's exit while its parent leaves SIGCHLD at SIG_DFL, unblocked: Linux
 * discards the notification, so nothing may interrupt or wake the parent.
 * Under ptrace Hermit, SIGCHLD Phase A holds turns until the kernel publishes
 * the exit, so the kernel's own SIGCHLD reaches Detcore at a fixed point. The
 * record/replay test checks that the replay receives it at the same point.
 *
 * Phase 1: the main thread is already blocked in read() on a pipe when the
 * child exits (the child sleeps first), and the process keeps the write end,
 * so the exit cannot produce EOF. A reaper thread waits for the child, sleeps,
 * then opens a futex gate; only then does a writer thread write one byte.
 *
 * Phase 2: the main thread waits for a second child in waitpid() itself (a
 * managed wait). */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fds[2];
static atomic_int gate;
static atomic_int reaped;
static pid_t child;

static void nap(long ns) {
  struct timespec ts = {0, ns};
  while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
  }
}

static void *reaper(void *arg) {
  (void)arg;
  int status;
  if (waitpid(child, &status, 0) != child) return (void *)1;
  atomic_store(&reaped, 1);
  nap(20000000);
  atomic_store(&gate, 1);
  syscall(SYS_futex, &gate, FUTEX_WAKE, 1, NULL, NULL, 0);
  return NULL;
}

static void *writer(void *arg) {
  (void)arg;
  while (atomic_load(&gate) == 0)
    syscall(SYS_futex, &gate, FUTEX_WAIT, 0, NULL, NULL, 0);
  if (write(fds[1], "w", 1) != 1) return (void *)1;
  return NULL;
}

static int sigchld_pending(void) {
  sigset_t pending;
  sigpending(&pending);
  return sigismember(&pending, SIGCHLD);
}

int main(void) {
  if (pipe(fds) != 0) return 2;
  child = fork();
  if (child < 0) return 3;
  if (child == 0) {
    nap(10000000);
    _exit(0);
  }
  pthread_t r, w;
  if (pthread_create(&w, NULL, writer, NULL) != 0) return 4;
  if (pthread_create(&r, NULL, reaper, NULL) != 0) return 5;
  char byte = 0;
  ssize_t got = read(fds[0], &byte, 1);
  int saved = errno;
  int was_reaped = atomic_load(&reaped);
  pthread_join(r, NULL);
  pthread_join(w, NULL);
  printf("read=%zd byte=%c errno=%s reaped_before_return=%d sigchld_pending=%d\n",
         got, got == 1 ? byte : '-', got < 0 ? strerror(saved) : "-", was_reaped,
         sigchld_pending());

  pid_t second = fork();
  if (second < 0) return 6;
  if (second == 0) {
    nap(10000000);
    _exit(3);
  }
  int status = 0;
  pid_t waited = waitpid(second, &status, 0);
  printf("waitpid=%s status=%d sigchld_pending=%d\n",
         waited == second ? "child" : strerror(errno),
         WIFEXITED(status) ? WEXITSTATUS(status) : -1, sigchld_pending());
  return got == 1 && byte == 'w' && was_reaped && waited == second ? 0 : 1;
}
