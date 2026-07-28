/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    pid_t foreground = 0;
    errno = 0;
    if (ioctl(STDERR_FILENO, TIOCGPGRP, &foreground) != -1 ||
        errno != ENOTTY) {
      _exit(2);
    }
    _exit(0);
  }

  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    return 1;
  }
  puts("dbi-copied-tiocgpgrp-ok");
  return 0;
}
