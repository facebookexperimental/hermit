/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

extern char** environ;

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "--after-exec") == 0)
    return puts("successful exec after failed exec") == EOF ? 8 : 0;
  if (argc > 2 || (argc == 2 && strcmp(argv[1], "--exec-self") != 0))
    return 1;
  char* const arguments[] = {"/definitely/missing", NULL};

  const struct timespec delay = {.tv_nsec = 1000000};
  // Every failed attempt must cancel the pending exec before this image
  // continues. A single failure never exercised a stale second PrepareExec.
  for (int attempt = 0; attempt < 2; ++attempt) {
    errno = 0;
    if (execve(arguments[0], arguments, environ) != -1)
      return 2;
    if (errno != ENOENT)
      return 3;
    if (nanosleep(&delay, NULL) != 0)
      return 4;
  }
  if (puts("recovered after failed exec") == EOF)
    return 5;
  if (argc == 2) {
    if (fflush(stdout) != 0)
      return 6;
    char* const next_arguments[] = {argv[0], "--after-exec", NULL};
    execve(argv[0], next_arguments, environ);
    return 7;
  }
  return 0;
}
