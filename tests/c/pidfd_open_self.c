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
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_pidfd_send_signal
#define SYS_pidfd_send_signal 424
#endif

#ifndef SYS_pidfd_getfd
#define SYS_pidfd_getfd 438
#endif

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

  const char payload[] = "pidfd-shared-description";
  int data_fd = memfd_create("hermit-pidfd", MFD_CLOEXEC);
  if (data_fd < 0 ||
      write(data_fd, payload, sizeof(payload)) != (ssize_t)sizeof(payload) ||
      lseek(data_fd, 0, SEEK_SET) != 0) {
    perror("prepare pidfd_getfd payload");
    close(fd);
    return 1;
  }
  int duplicate = (int)syscall(SYS_pidfd_getfd, fd, data_fd, 0);
  char readback[sizeof(payload)] = {0};
  int duplicate_flags = duplicate >= 0 ? fcntl(duplicate, F_GETFD) : -1;
  if (duplicate < 0 || duplicate_flags < 0 ||
      (duplicate_flags & FD_CLOEXEC) == 0 ||
      read(duplicate, readback, sizeof(readback)) !=
          (ssize_t)sizeof(readback) ||
      memcmp(readback, payload, sizeof(payload)) != 0 ||
      lseek(data_fd, 0, SEEK_CUR) != (off_t)sizeof(payload)) {
    fprintf(stderr, "pidfd_getfd did not preserve the open description: %s\n",
            strerror(errno));
    if (duplicate >= 0) {
      close(duplicate);
    }
    close(data_fd);
    close(fd);
    return 1;
  }
  close(duplicate);
  close(data_fd);

  if (syscall(SYS_pidfd_send_signal, fd, 0, NULL, 0) != 0) {
    perror("pidfd_send_signal signal 0");
    close(fd);
    return 1;
  }
  errno = 0;
  long invalid_signal = syscall(SYS_pidfd_send_signal, fd, 0, NULL, 1);
  if (invalid_signal != -1 || errno != EINVAL) {
    fprintf(stderr, "pidfd_send_signal invalid flags returned %ld/%d\n",
            invalid_signal, errno);
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
