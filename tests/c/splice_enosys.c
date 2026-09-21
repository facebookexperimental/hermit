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
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(int argc, char **argv) {
  int expect_passthrough = argc == 2 && strcmp(argv[1], "passthrough") == 0;
  int pipefd[2];
  if (pipe(pipefd) != 0 || write(pipefd[1], "x", 1) != 1) {
    perror("prepare splice pipe");
    return 2;
  }

  int sink = open("/dev/null", O_WRONLY);
  if (sink < 0) {
    perror("open /dev/null");
    return 2;
  }

  errno = 0;
  long result = syscall(SYS_splice, pipefd[0], NULL, sink, NULL, 1UL, 0U);
  if (expect_passthrough && result == 1) {
    puts("splice legacy passthrough preserved");
    return 0;
  }
  if (!expect_passthrough && result == -1 && errno == ENOSYS) {
    puts("splice deterministically unavailable");
    return 0;
  }
  {
    fprintf(stderr, "splice returned %ld with errno %d (%s), expected ENOSYS\n",
            result, errno, strerror(errno));
    return 1;
  }
}
