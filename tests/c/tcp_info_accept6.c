/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <arpa/inet.h>
#include <linux/tcp.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

static int fail(const char *operation) {
  perror(operation);
  return 1;
}

static int check_info(int fd) {
  struct tcp_info info = {0};
  socklen_t length = sizeof(info);
  if (getsockopt(fd, IPPROTO_TCP, TCP_INFO, &info, &length) < 0) {
    return fail("getsockopt TCP_INFO");
  }

  const unsigned char *bytes = (const unsigned char *)&info;
  for (size_t offset = 0; offset < length; ++offset) {
    if (offset != 0 && offset != 1 && offset != 5 && offset != 6 &&
        bytes[offset] != 0) {
      fprintf(stderr, "host TCP_INFO byte %zu is %#x\n", offset, bytes[offset]);
      return 2;
    }
  }
  printf("accept6 state=%u ca=%u options=%u scales=%u\n", bytes[0], bytes[1],
         bytes[5], bytes[6]);
  return 0;
}

int main(void) {
  int listener = socket(AF_INET6, SOCK_STREAM, 0);
  if (listener < 0) {
    return fail("socket listener");
  }
  struct sockaddr_in6 address = {
      .sin6_family = AF_INET6,
      .sin6_addr = IN6ADDR_LOOPBACK_INIT,
  };
  if (bind(listener, (struct sockaddr *)&address, sizeof(address)) < 0) {
    return fail("bind");
  }
  socklen_t address_length = sizeof(address);
  if (getsockname(listener, (struct sockaddr *)&address, &address_length) < 0) {
    return fail("getsockname");
  }
  if (listen(listener, 1) < 0) {
    return fail("listen");
  }

  int client = socket(AF_INET6, SOCK_STREAM, 0);
  if (client < 0) {
    return fail("socket client");
  }
  if (connect(client, (struct sockaddr *)&address, sizeof(address)) < 0) {
    return fail("connect");
  }
  int accepted = accept(listener, NULL, NULL);
  if (accepted < 0) {
    return fail("accept");
  }

  int result = check_info(accepted);
  close(accepted);
  close(client);
  close(listener);
  return result;
}
