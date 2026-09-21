/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <arpa/inet.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef SO_INCOMING_CPU
#define SO_INCOMING_CPU 49
#endif

static int fail(const char *operation) {
  perror(operation);
  return 1;
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
  socklen_t address_len = sizeof(address);
  if (getsockname(listener, (struct sockaddr *)&address, &address_len) < 0) {
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

  int cpu = -1;
  socklen_t cpu_len = sizeof(cpu);
  if (getsockopt(accepted, SOL_SOCKET, SO_INCOMING_CPU, &cpu, &cpu_len) < 0) {
    return fail("getsockopt SO_INCOMING_CPU");
  }
  if (cpu != 0) {
    fprintf(stderr, "expected virtual CPU 0, got %d\n", cpu);
    return 2;
  }
  printf("tcp6-incoming-cpu=%d\n", cpu);

  close(accepted);
  close(client);
  close(listener);
  return 0;
}
