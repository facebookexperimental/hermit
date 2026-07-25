/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

static int parse_count(const char *input, unsigned long *count) {
  char *end = NULL;
  errno = 0;
  *count = strtoul(input, &end, 10);
  return errno == 0 && end != input && *end == '\0' ? 0 : -1;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    return 2;
  }

  unsigned long iterations = 0;
  if (parse_count(argv[1], &iterations) != 0) {
    return 2;
  }

  volatile long sink = 0;
  for (unsigned long iteration = 0; iteration < iterations; iteration++) {
    sink += syscall(SYS_getpid);
  }

  return sink < 0;
}
