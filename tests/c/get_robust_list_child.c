/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <linux/futex.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  int ready_pipe[2];
  int release_pipe[2];
  if (pipe(ready_pipe) != 0 || pipe(release_pipe) != 0) {
    perror("pipe");
    return 1;
  }

  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    char byte = 1;
    if (write(ready_pipe[1], &byte, sizeof(byte)) != sizeof(byte) ||
        read(release_pipe[0], &byte, sizeof(byte)) != sizeof(byte)) {
      _exit(1);
    }
    _exit(0);
  }

  char byte = 0;
  if (read(ready_pipe[0], &byte, sizeof(byte)) != sizeof(byte)) {
    perror("child readiness");
    return 1;
  }

  struct robust_list_head *head = NULL;
  size_t length = 0;
  if (syscall(SYS_get_robust_list, child, &head, &length) != 0) {
    perror("get_robust_list child");
    return 1;
  }
  if (length != sizeof(*head)) {
    fprintf(stderr, "unexpected child robust-list length\n");
    return 1;
  }

  byte = 1;
  if (write(release_pipe[1], &byte, sizeof(byte)) != sizeof(byte)) {
    perror("child release");
    return 1;
  }

  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fprintf(stderr, "child failed\n");
    return 1;
  }

  puts("child robust list queried");
  return 0;
}
