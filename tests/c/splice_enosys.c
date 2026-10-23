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

int main(int argc, char** argv) {
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
  // The file name predates the refusal errno. A refused splice must say
  // EINVAL, the errno GNU grep (3.12) falls back to read(2) on; under ENOSYS
  // grep reports a read error and exits 2. Take that fallback here too.
  if (!expect_passthrough && result == -1 && errno == EINVAL) {
    char byte = 0;
    if (read(pipefd[0], &byte, 1) == 1 && byte == 'x') {
      puts("splice deterministically unavailable; read fallback got x");
      return 0;
    }
    perror("read fallback after splice EINVAL");
    return 1;
  }
  {
    fprintf(
        stderr,
        "splice returned %ld with errno %d (%s), expected EINVAL\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }
}
