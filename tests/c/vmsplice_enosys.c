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
#include <sys/uio.h>
#include <unistd.h>

int main(int argc, char **argv) {
  int expect_passthrough = argc == 2 && strcmp(argv[1], "passthrough") == 0;
  int pipefd[2];
  if (pipe(pipefd) != 0) {
    perror("prepare vmsplice pipe");
    return 2;
  }

  char byte = 'x';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  errno = 0;
  long result = syscall(SYS_vmsplice, pipefd[1], &iov, 1UL, 0U);
  if (expect_passthrough && result == 1) {
    char copied = 0;
    if (read(pipefd[0], &copied, 1) == 1 && copied == 'x') {
      puts("vmsplice legacy passthrough preserved");
      return 0;
    }
  }
  if (!expect_passthrough && result == -1 && errno == ENOSYS) {
    puts("vmsplice deterministically unavailable");
    return 0;
  }
  {
    fprintf(stderr,
            "vmsplice returned %ld with errno %d (%s), expected ENOSYS\n",
            result, errno, strerror(errno));
    return 1;
  }
}
