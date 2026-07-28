/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile sig_atomic_t delivered;

static void receive_signal(int signal, siginfo_t *info, void *context) {
  (void)signal;
  (void)context;
  if (info != NULL && info->si_code == SI_QUEUE)
    ++delivered;
}

static siginfo_t queued_signal(void) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  info.si_signo = SIGUSR1;
  info.si_code = SI_QUEUE;
  return info;
}

int main(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = receive_signal;
  action.sa_flags = SA_SIGINFO;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, NULL) != 0)
    return 1;

  pid_t pid = getpid();
  pid_t tid = (pid_t)syscall(SYS_gettid);
  siginfo_t info = queued_signal();
  long result = syscall(SYS_rt_tgsigqueueinfo, pid, tid, SIGUSR1, &info);
  if (result != 0 || delivered != 1) {
    fprintf(stderr,
            "rt_tgsigqueueinfo failed: result=%ld errno=%d delivered=%d\n",
            result, errno, delivered);
    return 2;
  }

  info = queued_signal();
  result = syscall(SYS_rt_sigqueueinfo, pid, SIGUSR1, &info);
  if (result != 0 || delivered != 2) {
    fprintf(stderr,
            "rt_sigqueueinfo failed: result=%ld errno=%d delivered=%d\n",
            result, errno, delivered);
    return 3;
  }

  puts("dbi-self-sigqueue-ok");
  return 0;
}
