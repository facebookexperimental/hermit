/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t sigchld_count;

static void on_sigchld(int signal_number) {
  (void)signal_number;
  ++sigchld_count;
}

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

int main(void) {
  struct sigaction action = {0};
  action.sa_handler = on_sigchld;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGCHLD, &action, NULL) != 0)
    fail("sigaction");

  pid_t first = fork();
  if (first < 0)
    fail("fork wait4");
  if (first == 0)
    _exit(7);

  int status = 0;
  struct rusage usage;
  const struct rusage zero_usage = {0};
  memset(&usage, 0xa5, sizeof(usage));
  if (wait4(first, &status, 0, &usage) != first)
    fail("wait4");
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 7)
    return 2;
  if (memcmp(&usage, &zero_usage, sizeof(usage)) != 0)
    return 3;

  pid_t second = fork();
  if (second < 0)
    fail("fork waitid");
  if (second == 0)
    _exit(9);

  siginfo_t info;
  memset(&info, 0, sizeof(info));
  if (waitid(P_PID, second, &info, WEXITED) != 0)
    fail("waitid");
  if (info.si_code != CLD_EXITED || info.si_pid != second ||
      info.si_status != 9)
    return 4;
  if (info.si_utime != 0 || info.si_stime != 0)
    return 5;
  if (sigchld_count != 2)
    return 6;

  if (waitpid(first, NULL, WNOHANG) != -1 || errno != ECHILD)
    return 7;
  if (waitpid(second, NULL, WNOHANG) != -1 || errno != ECHILD)
    return 8;

  printf("wait4=7 waitid=9 sigchld=2 reaped=2 cpu=zero\n");
  return 0;
}
