/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) {
    perror("socket");
    return 1;
  }

  socklen_t length = 0;
  if (getsockopt(fd, SOL_SOCKET, SO_TYPE, NULL, &length) < 0) {
    perror("getsockopt");
    close(fd);
    return 2;
  }
  if (length != 0) {
    fprintf(stderr, "unexpected option length: %u\n", (unsigned)length);
    close(fd);
    return 3;
  }

  close(fd);
  puts("getsockopt-null-ok");
  return 0;
}
