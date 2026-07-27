/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(int argc, char **argv) {
  int expect_passthrough = argc == 2 && strcmp(argv[1], "passthrough") == 0;
  int source[2];
  int destination[2];
  if (pipe(source) != 0 || pipe(destination) != 0 ||
      write(source[1], "x", 1) != 1) {
    perror("prepare tee pipes");
    return 2;
  }

  errno = 0;
  long result = syscall(SYS_tee, source[0], destination[1], 1UL, 0U);
  if (expect_passthrough && result == 1) {
    char original = 0;
    char copy = 0;
    if (read(source[0], &original, 1) == 1 &&
        read(destination[0], &copy, 1) == 1 && original == 'x' && copy == 'x') {
      puts("tee legacy passthrough preserved");
      return 0;
    }
  }
  if (!expect_passthrough && result == -1 && errno == ENOSYS) {
    puts("tee deterministically unavailable");
    return 0;
  }
  {
    fprintf(stderr, "tee returned %ld with errno %d (%s), expected ENOSYS\n",
            result, errno, strerror(errno));
    return 1;
  }
}
