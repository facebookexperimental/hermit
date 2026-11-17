/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A thread waits in sigsuspend for a SIGUSR1 another thread sends it. Linux
// interrupts the wait with -ERESTARTNOHAND, runs the handler and returns
// EINTR.

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile sig_atomic_t handled;
static pid_t waiter;

static void on_usr1(int sig) {
  (void)sig;
  handled++;
}

static void* send_usr1(void* arg) {
  (void)arg;
  syscall(SYS_tgkill, getpid(), waiter, SIGUSR1);
  return NULL;
}

int main(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigemptyset(&action.sa_mask);
  sigaction(SIGUSR1, &action, NULL);
  sigset_t usr1, empty;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  sigemptyset(&empty);
  sigprocmask(SIG_BLOCK, &usr1, NULL);
  waiter = (pid_t)syscall(SYS_gettid);
  pthread_t sender;
  pthread_create(&sender, NULL, send_usr1, NULL);
  int ret = sigsuspend(&empty);
  int error = errno;
  pthread_join(sender, NULL);
  printf("sigsuspend: %d (%s), handled: %d\n", ret, strerror(error), (int)handled);
  return ret == -1 && error == EINTR && handled == 1 ? 0 : 1;
}
