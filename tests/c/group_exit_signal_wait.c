/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// One thread waits for a signal that never comes while another thread of the
// process calls exit(0). On Linux exit_group ends every thread of the group,
// the waiter included, so the process exits 0 as soon as the line is printed.
//
//   worker-sigwaitinfo   a worker waits in sigwaitinfo; main exits
//   main-sigwaitinfo     main waits in sigwaitinfo; a worker exits
//   worker-sigtimedwait  a worker waits in sigtimedwait for 100 s; main exits
//
// The waiter writes one byte to a pipe immediately before it enters its wait,
// and the exiting thread reads that byte before its 100 ms delay, so the exit
// comes after the waiter has reached the wait. The wait must never return:
// any return ends the process with exit status 7, before the line is printed.
//
// Exit statuses other than 0: 2 a setup call failed, 3 a thread could not be
// created, 4 the readiness byte could not be written, 5 it could not be read,
// 7 the wait returned, 64 an unknown mode.

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static sigset_t waited;
static int timed;
static int ready[2];

__attribute__((noreturn)) static void wait_for_the_signal(void) {
  siginfo_t info;
  int result;
  char byte = 'w';
  if (write(ready[1], &byte, 1) != 1) {
    _exit(4);
  }
  if (timed) {
    struct timespec timeout = {100, 0};
    result = sigtimedwait(&waited, &info, &timeout);
  } else {
    result = sigwaitinfo(&waited, &info);
  }
  // No signal is ever sent and the timeout is far away, so any return,
  // success or error, means the wait ended without the group's exit.
  fprintf(
      stderr,
      "group_exit_signal_wait: the wait returned %d (errno %d)\n",
      result,
      errno);
  _exit(7);
}

__attribute__((noreturn)) static void exit_once_the_waiter_is_ready(
    const char* line) {
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    _exit(5);
  }
  usleep(100000);
  printf("%s\n", line);
  fflush(stdout);
  exit(0);
}

static void* waiting_worker(void* unused) {
  (void)unused;
  wait_for_the_signal();
  return NULL;
}

static void* exiting_worker(void* unused) {
  (void)unused;
  exit_once_the_waiter_is_ready(
      "worker: exiting while main waits in sigwaitinfo");
  return NULL;
}

int main(int argc, char** argv) {
  pthread_t thread;
  const char* mode = argc == 2 ? argv[1] : "";
  sigemptyset(&waited);
  sigaddset(&waited, SIGUSR2);
  if (pthread_sigmask(SIG_BLOCK, &waited, NULL) != 0 || pipe(ready) != 0) {
    return 2;
  }
  if (strcmp(mode, "main-sigwaitinfo") == 0) {
    if (pthread_create(&thread, NULL, exiting_worker, NULL) != 0) {
      return 3;
    }
    wait_for_the_signal();
  }
  if (strcmp(mode, "worker-sigwaitinfo") == 0 ||
      strcmp(mode, "worker-sigtimedwait") == 0) {
    timed = strcmp(mode, "worker-sigtimedwait") == 0;
    if (pthread_create(&thread, NULL, waiting_worker, NULL) != 0) {
      return 3;
    }
    exit_once_the_waiter_is_ready(
        timed ? "main: exiting while a worker waits in sigtimedwait"
              : "main: exiting while a worker waits in sigwaitinfo");
  }
  fprintf(
      stderr,
      "usage: %s worker-sigwaitinfo|main-sigwaitinfo|worker-sigtimedwait\n",
      argv[0]);
  return 64;
}
