/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static long parse_iterations(const char *value) {
  char *end = NULL;
  errno = 0;
  long iterations = strtol(value, &end, 10);
  if (errno != 0 || end == value || *end != '\0' || iterations < 0 ||
      iterations > INT_MAX) {
    fprintf(stderr, "invalid iteration count: %s\n", value);
    exit(2);
  }
  return iterations;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s ITERATIONS\n", argv[0]);
    return 2;
  }

  const long iterations = parse_iterations(argv[1]);
  for (long index = 0; index < iterations; ++index) {
    const pid_t child = fork();
    if (child < 0) {
      perror("fork");
      return 1;
    }
    if (child == 0) {
      execl("/bin/true", "true", (char *)NULL);
      perror("execl");
      _exit(127);
    }

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      perror("waitpid");
      return 1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
      fprintf(stderr, "child %ld failed with status %d\n", index, status);
      return 1;
    }
  }

  return 0;
}
