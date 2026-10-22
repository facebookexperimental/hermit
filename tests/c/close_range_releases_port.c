/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * close_range(3, ~0U, 0) closes every descriptor above stdio, a bound socket
 * included, and Hermit must account for it as for a close: the socket's
 * deterministic port goes back to the allocator, so the next bind to port 0
 * gets the same port. Under in-guest LiteInst the range also covers the
 * runtime's own coordinator connection; the call must still reach Detcore.
 */

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

static unsigned bind_ephemeral(void) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) {
    perror("socket");
    exit(1);
  }
  struct sockaddr_in address;
  memset(&address, 0, sizeof(address));
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  address.sin_port = 0;
  if (bind(fd, (struct sockaddr*)&address, sizeof(address)) != 0) {
    perror("bind");
    exit(1);
  }
  socklen_t length = sizeof(address);
  if (getsockname(fd, (struct sockaddr*)&address, &length) != 0) {
    perror("getsockname");
    exit(1);
  }
  return ntohs(address.sin_port);
}

int main(void) {
  unsigned first = bind_ephemeral();
  if (syscall(SYS_close_range, 3U, ~0U, 0U) != 0) {
    perror("close_range");
    return 1;
  }
  unsigned second = bind_ephemeral();
  printf("port reused=%d\n", first == second);
  return 0;
}
