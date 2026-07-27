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

int main(void) {
  int fd = (int)syscall(SYS_pidfd_open, getpid(), 0);
  if (fd < 0) {
    fprintf(stderr, "pidfd_open self failed: %s\n", strerror(errno));
    return 1;
  }
  int flags = fcntl(fd, F_GETFD);
  if (flags < 0 || (flags & FD_CLOEXEC) == 0) {
    fprintf(stderr, "pidfd missing FD_CLOEXEC: flags=%d errno=%d\n", flags,
            errno);
    close(fd);
    return 1;
  }
  close(fd);

  errno = 0;
  int invalid = (int)syscall(SYS_pidfd_open, getpid(), 1);
  if (invalid != -1 || errno != EINVAL) {
    fprintf(stderr, "pidfd_open invalid flags returned %d/%d\n", invalid,
            errno);
    return 1;
  }
  puts("pidfd-open-self-ok cloexec=1");
  return 0;
}
