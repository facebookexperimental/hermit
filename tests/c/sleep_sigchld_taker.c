/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/external_signal_interrupt.rs
 * (https://github.com/rrnewton/hermit/issues/4333).
 *
 * usage: sleep_sigchld_taker <pause|nanosleep>
 *
 * The main thread catches SIGCHLD, starts a sibling thread that spins without
 * system calls until a SIGCHLD handler has run, and forks a child that exits
 * after 100 ms. It then sleeps: in pause(), or in a 10 s nanosleep. Neither
 * thread blocks SIGCHLD, and Linux gives the child's SIGCHLD to the thread
 * that forked it, which is sleeping: the sleep ends with EINTR and the handler
 * runs on the main thread. The guest prints
 *   mode=<mode> result=<ret> errno=<errno> handler_main=<n> handler_sibling=<n>
 * and exits 0. If the sibling took the signal instead, a pause never returns
 * and a nanosleep runs to its deadline and returns 0.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static _Atomic int handled_main;
static _Atomic int handled_sibling;
static pid_t main_tid;

static void on_chld(int sig) {
  (void)sig;
  if ((pid_t)syscall(SYS_gettid) == main_tid) {
    atomic_fetch_add(&handled_main, 1);
  } else {
    atomic_fetch_add(&handled_sibling, 1);
  }
}

static void* spin(void* arg) {
  (void)arg;
  /* Bounded, so a guest that never sees the handler still ends. */
  for (unsigned long n = 0; n < 20000000000UL; n++) {
    if (atomic_load(&handled_main) + atomic_load(&handled_sibling) > 0) {
      break;
    }
  }
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 2 ||
      (strcmp(argv[1], "pause") != 0 && strcmp(argv[1], "nanosleep") != 0)) {
    fprintf(stderr, "usage: %s <pause|nanosleep>\n", argv[0]);
    return 2;
  }
  const char* mode = argv[1];
  main_tid = (pid_t)syscall(SYS_gettid);
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_handler = on_chld;
  if (sigaction(SIGCHLD, &sa, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  pthread_t sibling;
  if (pthread_create(&sibling, NULL, spin, NULL) != 0) {
    fprintf(stderr, "pthread_create failed\n");
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    struct timespec delay = {0, 100 * 1000 * 1000};
    nanosleep(&delay, NULL);
    _exit(0);
  }
  int result;
  int err;
  if (strcmp(mode, "nanosleep") == 0) {
    struct timespec duration = {10, 0};
    result = nanosleep(&duration, NULL);
  } else {
    result = pause();
  }
  err = result < 0 ? errno : 0;
  pthread_join(sibling, NULL);
  if (waitpid(child, NULL, 0) != child) {
    perror("waitpid");
    return 1;
  }
  printf(
      "mode=%s result=%d errno=%d handler_main=%d handler_sibling=%d\n",
      mode,
      result,
      err,
      atomic_load(&handled_main),
      atomic_load(&handled_sibling));
  return 0;
}
