/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Threads held at a happens-before gate while something else happens to them
 * (https://github.com/rrnewton/hermit/pull/3897). hermit-cli/tests/cli.rs runs
 * one mode per test:
 *
 *   alarm    Catches SIGALRM, arms alarm(1) and makes 200 getpid calls. The
 *            test holds the thread at a gate that can never open, so the alarm
 *            is sent to a held thread and the run must end in a deadlock
 *            report rather than a scheduler panic and a hang.
 *   exit     A worker thread makes 100 getpid calls and writes; main makes 20
 *            getppid calls, writes "main-exits\n" and calls _exit(0). The test
 *            holds the worker at a gate that can never open, so the process
 *            exits with the worker still held. The run must exit 0.
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile sig_atomic_t caught;

static void on_signal(int signo) {
  (void)signo;
  caught = 1;
}

static void catch_signal(int signo) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_signal;
  action.sa_flags = SA_RESTART;
  sigaction(signo, &action, NULL);
}

static int run_alarm(void) {
  catch_signal(SIGALRM);
  alarm(1);
  for (int i = 0; i < 200; i++) {
    syscall(SYS_getpid);
  }
  write(1, "done\n", 5);
  return 0;
}

static void* worker(void* arg) {
  (void)arg;
  for (int i = 0; i < 100; i++) {
    syscall(SYS_getpid);
  }
  write(1, "worker\n", 7);
  return NULL;
}

static int run_exit(void) {
  pthread_t thread;
  pthread_create(&thread, NULL, worker, NULL);
  for (int i = 0; i < 20; i++) {
    syscall(SYS_getppid);
  }
  write(1, "main-exits\n", 11);
  _exit(0);
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "alarm") == 0) {
    return run_alarm();
  }
  if (argc == 2 && strcmp(argv[1], "exit") == 0) {
    return run_exit();
  }
  return 2;
}
