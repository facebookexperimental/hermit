/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

static int enable_reuse(int fd) {
  int enabled = 1;
  if (setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof(enabled)) < 0) {
    perror("setsockopt(SO_REUSEADDR)");
    return -1;
  }
  if (setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &enabled, sizeof(enabled)) < 0) {
    perror("setsockopt(SO_REUSEPORT)");
    return -1;
  }
  return 0;
}

int main(void) {
  int first = socket(AF_INET, SOCK_STREAM, 0);
  int second = socket(AF_INET, SOCK_STREAM, 0);
  if (first < 0 || second < 0) {
    perror("socket");
    return 1;
  }
  if (enable_reuse(first) < 0 || enable_reuse(second) < 0) {
    return 2;
  }

  struct sockaddr_in address = {
      .sin_family = AF_INET,
      .sin_port = 0,
      .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
  };
  if (bind(first, (struct sockaddr *)&address, sizeof(address)) < 0) {
    perror("bind(first)");
    return 3;
  }
  socklen_t address_len = sizeof(address);
  if (getsockname(first, (struct sockaddr *)&address, &address_len) < 0) {
    perror("getsockname(first)");
    return 4;
  }

  if (address.sin_port == 0) {
    fputs("socket did not receive an assigned port\n", stderr);
    return 5;
  }
  if (bind(second, (struct sockaddr *)&address, sizeof(address)) < 0) {
    perror("bind(second)");
    return 5;
  }
  if (listen(first, 1) < 0 || listen(second, 1) < 0) {
    perror("listen");
    return 6;
  }

  close(second);
  close(first);
  puts("setsockopt-replay-ok");
  return 0;
}
