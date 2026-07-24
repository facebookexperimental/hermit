/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

int main(void) {
  char *const arguments[] = {"/definitely/missing", NULL};

  errno = 0;
  if (execve(arguments[0], arguments, environ) != -1)
    return 2;
  if (errno != ENOENT)
    return 3;
  const struct timespec delay = {.tv_nsec = 1000000};
  if (nanosleep(&delay, NULL) != 0)
    return 4;
  if (puts("recovered after failed exec") == EOF)
    return 5;
  return 0;
}
