/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A parent waits for a child that cannot exit, or for one that will
// (https://github.com/rrnewton/hermit/issues/4430). Modes:
//   pause   the child pauses forever: nothing can end the wait
//   futex   the child waits forever on a futex no one wakes
//   chain   the child waits for a grandchild that pauses forever
//   killer  the child pauses, and a second parent thread kills it after a
//           one-second sleep: the wait ends with SIGKILL
//   alarm   the child pauses with a caught SIGALRM from alarm(1), then exits
//           3: the wait ends with status 3
// Prints "<mode>: child signal <n>" or "<mode>: child exit <n>" when the
// wait ends.
#define _GNU_SOURCE
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t child;

static void* killer(void* arg) {
  (void)arg;
  sleep(1);
  kill(child, SIGKILL);
  return NULL;
}

static void on_alarm(int sig) {
  (void)sig;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "pause";
  child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    if (!strcmp(mode, "futex")) {
      int word = 0;
      syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
      _exit(1);
    }
    if (!strcmp(mode, "chain")) {
      pid_t grandchild = fork();
      if (grandchild == 0) {
        for (;;) {
          pause();
        }
      }
      waitpid(grandchild, NULL, 0);
      _exit(2);
    }
    if (!strcmp(mode, "alarm")) {
      signal(SIGALRM, on_alarm);
      alarm(1);
      pause();
      _exit(3);
    }
    for (;;) {
      pause();
    }
  }
  pthread_t thread;
  if (!strcmp(mode, "killer") && pthread_create(&thread, NULL, killer, NULL) != 0) {
    return 1;
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  printf("%s: child %s %d\n", mode, WIFSIGNALED(status) ? "signal" : "exit",
         WIFSIGNALED(status) ? WTERMSIG(status) : WEXITSTATUS(status));
  return 0;
}
