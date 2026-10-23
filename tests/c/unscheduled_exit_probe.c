/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Processes that die at a moment the host chooses, without exiting through a
 * system call of their own. A run must still complete:
 *
 *   pdeathsig            a child armed with PR_SET_PDEATHSIG(SIGKILL) is killed
 *                        while its parent exits;
 *   vfork-parent-killed  a vfork child kills its parent; the grandparent, the
 *                        surviving work, reaps the parent and goes on.
 *
 * Each line printed is fixed, so the output is the same in every run. Whether
 * the runs' records can be compared is the backend's decision: a backend that
 * cannot schedule such a death records a determinism loss.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  exit(1);
}

static void say(const char* line) {
  if (write(STDOUT_FILENO, line, strlen(line)) < 0) {
    _exit(1);
  }
}

static int pdeathsig(void) {
  int armed[2];
  if (pipe(armed) != 0) {
    fail("pipe");
  }
  pid_t parent = fork();
  if (parent < 0) {
    fail("fork");
  }
  if (parent == 0) {
    pid_t child = fork();
    if (child < 0) {
      fail("fork");
    }
    if (child == 0) {
      if (prctl(PR_SET_PDEATHSIG, SIGKILL) != 0) {
        fail("prctl");
      }
      if (write(armed[1], "a", 1) != 1) {
        fail("write");
      }
      /* Sleeps until the parent's exit kills it. */
      for (;;) {
        pause();
      }
    }
    char byte;
    if (read(armed[0], &byte, 1) != 1) {
      fail("read");
    }
    say("parent exits with its child armed\n");
    _exit(0);
  }
  close(armed[1]);
  int status = 0;
  if (waitpid(parent, &status, 0) != parent) {
    fail("waitpid");
  }
  /* EOF once the armed child is gone too: it held the only write end. */
  char byte;
  ssize_t got = read(armed[0], &byte, 1);
  printf("pdeathsig parent-status=%d armed-child-gone=%d\n",
         WIFEXITED(status) ? WEXITSTATUS(status) : -1, got == 0);
  return 0;
}

static int vfork_parent_killed(void) {
  pid_t parent = fork();
  if (parent < 0) {
    fail("fork");
  }
  if (parent == 0) {
    pid_t child = vfork();
    if (child < 0) {
      _exit(2);
    }
    if (child == 0) {
      kill(getppid(), SIGKILL);
      _exit(0);
    }
    /* Not reached when the kill lands first; a parent that runs on still
       ends here. */
    for (;;) {
      pause();
    }
  }
  int status = 0;
  if (waitpid(parent, &status, 0) != parent) {
    fail("waitpid");
  }
  printf("vfork-parent-killed parent-signal=%d surviving-work-done=1\n",
         WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "pdeathsig") == 0) {
    return pdeathsig();
  }
  if (argc == 2 && strcmp(argv[1], "vfork-parent-killed") == 0) {
    return vfork_parent_killed();
  }
  fprintf(stderr, "usage: %s pdeathsig|vfork-parent-killed\n", argv[0]);
  return 2;
}
