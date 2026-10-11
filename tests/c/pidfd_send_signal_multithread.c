/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * pidfd_send_signal to the caller's own process, first with one thread and
 * then with a second thread waiting in sigsuspend()
 * (https://github.com/rrnewton/hermit/issues/4047). A pidfd names a whole
 * process, so this is a process-directed signal: Linux's complete_signal picks
 * the receiving thread, which Detcore does not model, so with two threads it
 * refuses the call by name as it refuses kill. A fail-closed run stops with
 * the policy-refusal status after the one-thread block, and
 * --allow-unsupported-syscalls returns ENOSYS. Signal 0 only checks that the
 * process exists, so it is answered with two threads too.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile sig_atomic_t handled;

static void on_usr1(int sig) {
  (void)sig;
  handled++;
}

static void report(const char* what, long ret) {
  if (ret == 0) {
    printf("%s: 0\n", what);
  } else {
    printf("%s: %ld %s\n", what, ret, strerrorname_np(errno));
  }
}

static volatile int stop;

/* The second thread waits with SIGUSR1 blocked except inside sigsuspend, so
 * the final pthread_kill cannot slip in between its check and its wait. */
static void* waiter(void* unused) {
  (void)unused;
  sigset_t usr1, none;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  sigemptyset(&none);
  pthread_sigmask(SIG_BLOCK, &usr1, NULL);
  while (!stop) {
    sigsuspend(&none);
  }
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigaction(SIGUSR1, &action, NULL);
  int pidfd = syscall(SYS_pidfd_open, getpid(), 0);
  if (pidfd < 0) {
    perror("pidfd_open");
    return 2;
  }

  report("one thread, pidfd_send_signal", syscall(SYS_pidfd_send_signal, pidfd, SIGUSR1, NULL, 0));
  printf("one thread, handled: %d\n", (int)handled);

  pthread_t thread;
  pthread_create(&thread, NULL, waiter, NULL);
  report("two threads, pidfd_send_signal 0", syscall(SYS_pidfd_send_signal, pidfd, 0, NULL, 0));
  report("two threads, pidfd_send_signal", syscall(SYS_pidfd_send_signal, pidfd, SIGUSR1, NULL, 0));

  stop = 1;
  pthread_kill(thread, SIGUSR1);
  pthread_join(thread, NULL);
  return 0;
}
