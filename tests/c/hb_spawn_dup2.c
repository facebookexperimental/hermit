/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * posix_spawn with a dup2 file action: glibc implements posix_spawn as
 * clone(CLONE_VM | CLONE_VFORK), so the vfork child calls dup2(5, 7) before
 * execve, and the parent then writes "parent\n" to fd 1. hermit-cli/tests/cli.rs
 * uses it to check that a --happens-before anchor that would hold the vfork
 * child before its exec is refused by name rather than left to spin
 * (https://github.com/rrnewton/hermit/issues/3930); the program is the
 * reproduction from the review of https://github.com/rrnewton/hermit/pull/3928.
 */
#include <fcntl.h>
#include <spawn.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

int main(void) {
  int fd = open("/dev/null", O_WRONLY);
  if (fd != 5 && dup2(fd, 5) != 5) {
    return 9;
  }
  posix_spawn_file_actions_t actions;
  posix_spawn_file_actions_init(&actions);
  posix_spawn_file_actions_adddup2(&actions, 5, 7);
  char *argv[] = {"/bin/true", 0};
  pid_t pid;
  if (posix_spawn(&pid, "/bin/true", &actions, 0, argv, environ) != 0) {
    return 3;
  }
  if (write(1, "parent\n", 7) != 7) {
    return 2;
  }
  int status;
  waitpid(pid, &status, 0);
  return 0;
}
