/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int expect_execveat_enosys(const char *path, char *const arguments[]) {
  errno = 0;
  if (syscall(SYS_execveat, AT_FDCWD, path, arguments, environ, 0) != -1)
    return 1;
  return errno == ENOSYS ? 0 : 1;
}

int main(void) {
  char *const root_arguments[] = {"/bin/true", NULL};
  if (expect_execveat_enosys(root_arguments[0], root_arguments) != 0)
    return 2;

  pid_t child = fork();
  if (child < 0)
    return 3;
  if (child == 0) {
    char *const child_arguments[] = {"/bin/sh", "-c", "exit 42", NULL};
    if (expect_execveat_enosys(child_arguments[0], child_arguments) != 0)
      _exit(4);
    _exit(0);
  }

  int status = 0;
  if (waitpid(child, &status, 0) != child)
    return 5;
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
    return 6;
  if (puts("execveat unsupported in root and fork child") == EOF)
    return 7;
  return 0;
}
