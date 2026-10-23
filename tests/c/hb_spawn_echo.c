/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The parent posix_spawns /bin/echo, which writes "child\n", then writes
 * "parent\n" and waits for it. hermit-cli/tests/cli.rs orders the parent's
 * write before the child's with a happens-before edge, once firing at the
 * write's entry and once at its completion
 * (https://github.com/rrnewton/hermit/issues/3929).
 */
#include <spawn.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

int main(void) {
  char *argv[] = {"/bin/echo", "child", 0};
  pid_t pid;
  if (posix_spawn(&pid, "/bin/echo", 0, 0, argv, environ) != 0) {
    return 3;
  }
  if (write(1, "parent\n", 7) != 7) {
    return 2;
  }
  int status;
  if (waitpid(pid, &status, 0) != pid) {
    return 4;
  }
  return 0;
}
