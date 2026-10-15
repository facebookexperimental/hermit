/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A thread is signaled by a sibling that exits right after sending
// (https://github.com/rrnewton/hermit/issues/4367).
//
// A worker sleeps 10 ms, sends SIGUSR1 to the main thread with pthread_kill,
// and returns, so its exit comes while the main thread is taking the signal.
// The main thread waits for the handler:
//   spin        calling getpid in a loop;
//   sigsuspend  in a sigsuspend loop with SIGUSR1 otherwise blocked.
// Linux runs the handler and both threads finish: "<mode>: woken".
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t flag;
static pthread_t main_thread;

static void on_usr1(int sig) {
  (void)sig;
  flag = 1;
}

static void* worker(void* arg) {
  (void)arg;
  struct timespec ten_ms = {0, 10000000};
  nanosleep(&ten_ms, NULL);
  pthread_kill(main_thread, SIGUSR1);
  return NULL;
}

int main(int argc, char** argv) {
  int suspend = argc > 1 && strcmp(argv[1], "sigsuspend") == 0;
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigaction(SIGUSR1, &action, NULL);
  sigset_t usr1, unblocked;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  if (suspend) {
    sigprocmask(SIG_BLOCK, &usr1, &unblocked);
  }
  main_thread = pthread_self();
  pthread_t thread;
  pthread_create(&thread, NULL, worker, NULL);
  while (!flag) {
    if (suspend) {
      sigsuspend(&unblocked);
    } else {
      syscall(SYS_getpid);
    }
  }
  pthread_join(thread, NULL);
  printf("%s: woken\n", suspend ? "sigsuspend" : "spin");
  return 0;
}
