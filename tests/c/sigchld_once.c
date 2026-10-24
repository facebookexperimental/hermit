/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// One child exit gives its parent one SIGCHLD, with the siginfo Linux fills
// in: CLD_EXITED, the child's pid and its exit status. Under SaBRe the parent
// got two, the scheduler's copy and the kernel's, until Hermit's child-exit
// ledger covered backends that pass the tool the siginfo
// (dev-hermit minibeads dh-l1fk). Prints "OK one SIGCHLD per child exit" and
// exits 0, or prints what differed and exits 1.

#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t deliveries;
static volatile sig_atomic_t first_code;
static volatile sig_atomic_t first_pid;
static volatile sig_atomic_t first_status;

static void on_chld(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  if (deliveries == 0) {
    first_code = info->si_code;
    first_pid = info->si_pid;
    first_status = info->si_status;
  }
  deliveries++;
}

int main(void) {
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_sigaction = on_chld;
  action.sa_flags = SA_SIGINFO | SA_RESTART;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 2;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 2;
  }
  if (child == 0) {
    _exit(7);
  }
  // Long enough for the child to exit, and for any second copy to arrive,
  // before and after the wait.
  struct timespec pause = {0, 100000000};
  nanosleep(&pause, NULL);
  int status = 0;
  pid_t reaped = waitpid(child, &status, 0);
  nanosleep(&pause, NULL);
  int failures = 0;
  if (reaped != child || !WIFEXITED(status) || WEXITSTATUS(status) != 7) {
    printf("FAIL waitpid: %d, status %#x\n", (int)reaped, status);
    failures++;
  }
  if (deliveries != 1) {
    printf("FAIL deliveries: %d, expected 1\n", (int)deliveries);
    failures++;
  }
  if (first_code != CLD_EXITED || first_pid != child || first_status != 7) {
    printf(
        "FAIL siginfo: si_code %d si_pid %d si_status %d, expected %d %d 7\n",
        (int)first_code,
        (int)first_pid,
        (int)first_status,
        CLD_EXITED,
        (int)child);
    failures++;
  }
  if (failures) {
    return 1;
  }
  printf("OK one SIGCHLD per child exit\n");
  return 0;
}
