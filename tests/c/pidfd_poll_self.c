/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
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

  struct pollfd item = {.fd = fd, .events = POLLIN};
  int result = poll(&item, 1, 0);
  if (result != 0 || item.revents != 0) {
    fprintf(stderr, "live self pidfd was ready: result=%d revents=%d\n",
            result, item.revents);
    close(fd);
    return 1;
  }
  close(fd);
  puts("pidfd-poll-self-ok ready=0");
  return 0;
}
