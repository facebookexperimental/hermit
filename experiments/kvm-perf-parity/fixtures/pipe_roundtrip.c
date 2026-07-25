/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdlib.h>
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

  int descriptors[2];
  if (pipe(descriptors) != 0) {
    return 3;
  }

  const unsigned char expected = 0x5a;
  for (unsigned long iteration = 0; iteration < iterations; iteration++) {
    unsigned char actual = 0;
    if (write(descriptors[1], &expected, sizeof(expected)) !=
            sizeof(expected) ||
        read(descriptors[0], &actual, sizeof(actual)) != sizeof(actual) ||
        actual != expected) {
      return 4;
    }
  }

  if (close(descriptors[0]) != 0 || close(descriptors[1]) != 0) {
    return 5;
  }
  return 0;
}
