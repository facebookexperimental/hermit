/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The main thread catches SIGUSR1, with SA_RESTART when argv[1] is "restart"
 * and without it otherwise, and makes an untimed shared FUTEX_WAIT. glibc's
 * own waits use the private operations, so hermit-cli/tests/cli.rs can name
 * this one wait by its operation. A helper thread interrupts the wait with
 * SIGUSR1, then sets the word, wakes the waiter and writes "helper\n". The
 * main thread writes "main\n" once the word is set
 * (https://github.com/rrnewton/hermit/issues/3929). In "other-thread" mode
 * (with SA_RESTART) the helper also makes one shared FUTEX_WAIT of its own,
 * which fails with EAGAIN at once, before it sets the word: a posthook anchor
 * on the helper's wait must not be refused for the main thread's interrupted
 * one (the Claude review of https://github.com/rrnewton/hermit/pull/4048).
 */
#define _GNU_SOURCE
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static volatile int word;
static volatile int other_word;
static int other_thread;
static pid_t main_tid;

static void on_signal(int signo) {
  (void)signo;
}

static void* helper(void* arg) {
  (void)arg;
  struct timespec pause = {0, 1000000};
  nanosleep(&pause, 0);
  syscall(SYS_tgkill, getpid(), main_tid, SIGUSR1);
  nanosleep(&pause, 0);
  if (other_thread) {
    syscall(SYS_futex, &other_word, FUTEX_WAIT, 1, 0, 0, 0);
  }
  __atomic_store_n(&word, 1, __ATOMIC_SEQ_CST);
  syscall(SYS_futex, &word, FUTEX_WAKE, 1, 0, 0, 0);
  if (write(1, "helper\n", 7) != 7) {
    return (void*)1;
  }
  return 0;
}

int main(int argc, char** argv) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_signal;
  other_thread = argc > 1 && strcmp(argv[1], "other-thread") == 0;
  action.sa_flags =
      argc > 1 && (strcmp(argv[1], "restart") == 0 || other_thread) ? SA_RESTART : 0;
  sigaction(SIGUSR1, &action, 0);
  main_tid = (pid_t)syscall(SYS_gettid);
  pthread_t thread;
  if (pthread_create(&thread, 0, helper, 0) != 0) {
    return 3;
  }
  while (__atomic_load_n(&word, __ATOMIC_SEQ_CST) == 0) {
    syscall(SYS_futex, &word, FUTEX_WAIT, 0, 0, 0, 0);
  }
  if (write(1, "main\n", 5) != 5) {
    return 2;
  }
  pthread_join(thread, 0);
  return 0;
}
