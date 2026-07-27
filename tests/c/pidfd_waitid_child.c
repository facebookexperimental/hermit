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
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef P_PIDFD
#define P_PIDFD ((idtype_t)3)
#endif

int main(void) {
  int gate[2];
  if (pipe(gate) != 0) {
    perror("pipe");
    return 1;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  if (child == 0) {
    close(gate[1]);
    char byte;
    if (read(gate[0], &byte, 1) != 1) {
      _exit(1);
    }
    _exit(42);
  }
  close(gate[0]);

  int fd = (int)syscall(SYS_pidfd_open, child, O_NONBLOCK);
  if (fd < 0) {
    fprintf(stderr, "pidfd_open child failed: %s\n", strerror(errno));
    return 1;
  }

  siginfo_t info = {0};
  errno = 0;
  if (waitid(P_PIDFD, (id_t)fd, &info, WEXITED) != -1 || errno != EAGAIN) {
    fprintf(stderr, "nonblocking waitid returned errno=%d, expected EAGAIN\n",
            errno);
    close(fd);
    return 1;
  }

  char byte = 1;
  if (write(gate[1], &byte, 1) != 1) {
    perror("write gate");
    close(fd);
    return 1;
  }
  close(gate[1]);
  int status;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 42) {
    fprintf(stderr, "unexpected child wait status=%d errno=%d\n", status,
            errno);
    close(fd);
    return 1;
  }
  close(fd);
  puts("pidfd-waitid-child-ok eagain=1 status=42");
  return 0;
}
