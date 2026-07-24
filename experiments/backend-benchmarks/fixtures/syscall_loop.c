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
#include <sys/syscall.h>
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
  volatile long accumulator = 0;
  for (long index = 0; index < iterations; ++index) {
    accumulator += syscall(SYS_getpid);
  }

  if (accumulator < 0) {
    return 1;
  }
  return 0;
}
