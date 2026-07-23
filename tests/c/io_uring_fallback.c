/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <linux/io_uring.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <unistd.h>

static void expect_enosys(const char* name, long result) {
  if (result != -1 || errno != ENOSYS) {
    fprintf(
        stderr,
        "%s returned %ld with errno %d (%s), expected ENOSYS\n",
        name,
        result,
        errno,
        strerror(errno));
    exit(1);
  }
}

int main(void) {
  struct io_uring_params params = {0};

  errno = 0;
  expect_enosys(
      "io_uring_setup", syscall(SYS_io_uring_setup, 8, &params));

  errno = 0;
  expect_enosys(
      "io_uring_enter",
      syscall(SYS_io_uring_enter, -1, 0, 0, 0, NULL, 0));

  errno = 0;
  expect_enosys(
      "io_uring_register",
      syscall(SYS_io_uring_register, -1, 0, NULL, 0));

  int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
  if (epoll_fd < 0) {
    perror("epoll_create1 fallback");
    return 1;
  }
  close(epoll_fd);
  puts("io_uring blocked; epoll fallback ready");
  return 0;
}
