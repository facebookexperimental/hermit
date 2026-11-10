/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * sleep_ignored_sigchld MODE: the process forks a child that exits at once,
 * then sleeps 2 ms. The child's exit sends the parent SIGCHLD, which MODE
 * leaves at its default action, ignore ("default"), sets to SIG_IGN
 * ("ignore"), or blocks ("block"). Linux ends a sleep only for a signal it
 * delivers to a handler or that kills or stops the process, so in every mode
 * the sleep returns 0.
 *
 * Output: "rc=0", or "rc=EINTR" (or another error's name).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "default";
  if (strcmp(mode, "ignore") == 0) {
    signal(SIGCHLD, SIG_IGN);
  } else if (strcmp(mode, "block") == 0) {
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    sigprocmask(SIG_BLOCK, &chld, NULL);
  } else if (strcmp(mode, "default") != 0) {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    _exit(0);
  }
  struct timespec req = {0, 2000000};
  int rc = nanosleep(&req, NULL);
  int err = errno;
  /* Under SIG_IGN the kernel reaps the child itself. */
  if (strcmp(mode, "ignore") != 0) {
    waitpid(child, NULL, 0);
  }
  if (rc == 0) {
    printf("rc=0\n");
  } else {
    printf("rc=%s\n", err == EINTR ? "EINTR" : strerror(err));
  }
  return 0;
}
