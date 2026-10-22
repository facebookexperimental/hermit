/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Runs the program named by its first argument with posix_spawn, which glibc
 * implements with a vfork-like clone whose child execs, and prints how the
 * child exited. The child inherits standard output. */

#include <spawn.h>
#include <stdio.h>
#include <sys/wait.h>

extern char **environ;

int main(int argc, char **argv) {
  if (argc != 2)
    return 2;
  char *child_argv[] = {argv[1], NULL};
  pid_t pid;
  if (posix_spawn(&pid, argv[1], NULL, NULL, child_argv, environ) != 0)
    return 3;
  int status = 0;
  if (waitpid(pid, &status, 0) != pid)
    return 4;
  if (WIFEXITED(status))
    printf("child exited %d\n", WEXITSTATUS(status));
  else
    printf("child killed by signal %d\n", WTERMSIG(status));
  return 0;
}
