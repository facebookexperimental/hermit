/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * What a parent observes after a child's exit, in three shapes:
 *
 *   sigign     SIGCHLD set to SIG_IGN: the kernel auto-reaps and sends no
 *              SIGCHLD, so wait reports ECHILD and nothing is pending.
 *   nocldwait  SA_NOCLDWAIT: the kernel auto-reaps but still sends SIGCHLD,
 *              so wait reports ECHILD and SIGCHLD is pending.
 *   rawexit    the child ends with the raw exit system call; the parent
 *              collects status 7 and finds SIGCHLD pending.
 *
 * The parent blocks SIGCHLD, and learns of the exit by reading a pipe whose
 * only write end the child holds: EOF means the child's descriptors are
 * closed. A backend that completes exits asynchronously must not let the
 * parent run again until the exit is complete, SIGCHLD included, so every
 * line below is the same in every run.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  exit(1);
}

static int sigchld_pending(void) {
  sigset_t pending;
  if (sigpending(&pending) != 0) {
    fail("sigpending");
  }
  return sigismember(&pending, SIGCHLD);
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s sigign|nocldwait|rawexit\n", argv[0]);
    return 2;
  }
  const char* mode = argv[1];
  int sigign = strcmp(mode, "sigign") == 0;
  int nocldwait = strcmp(mode, "nocldwait") == 0;
  int rawexit = strcmp(mode, "rawexit") == 0;
  if (!sigign && !nocldwait && !rawexit) {
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
  }

  sigset_t chld;
  sigemptyset(&chld);
  sigaddset(&chld, SIGCHLD);
  if (sigprocmask(SIG_BLOCK, &chld, NULL) != 0) {
    fail("sigprocmask");
  }
  if (sigign || nocldwait) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = sigign ? SIG_IGN : SIG_DFL;
    action.sa_flags = nocldwait ? SA_NOCLDWAIT : 0;
    if (sigaction(SIGCHLD, &action, NULL) != 0) {
      fail("sigaction");
    }
  }

  for (int round = 0; round < 3; round++) {
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
      fail("pipe");
    }
    pid_t child = fork();
    if (child < 0) {
      fail("fork");
    }
    if (child == 0) {
      close(pipe_fds[0]);
      if (rawexit) {
        syscall(SYS_exit, 7);
      }
      _exit(7);
    }
    close(pipe_fds[1]);
    char byte;
    ssize_t got = read(pipe_fds[0], &byte, 1);
    close(pipe_fds[0]);
    int pending = sigchld_pending();
    int status = 0;
    pid_t waited = waitpid(child, &status, 0);
    const char* wait_result = waited == child ? "child"
        : (waited < 0 && errno == ECHILD) ? "ECHILD"
                                          : "other";
    printf(
        "%s round=%d read=%zd sigchld_pending=%d wait=%s status=%d\n",
        mode,
        round,
        got,
        pending,
        wait_result,
        waited == child && WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    /* Consume this round's SIGCHLD, if any, before the next. */
    struct timespec zero = {0, 0};
    while (sigtimedwait(&chld, NULL, &zero) == SIGCHLD) {
    }
  }
  return 0;
}
