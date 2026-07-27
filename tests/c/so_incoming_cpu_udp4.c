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
  int receiver = socket(AF_INET, SOCK_DGRAM, 0);
  if (receiver < 0) {
    return fail("socket receiver");
  }

  struct sockaddr_in address = {
      .sin_family = AF_INET,
      .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
  };
  if (bind(receiver, (struct sockaddr *)&address, sizeof(address)) < 0) {
    return fail("bind");
  }
  socklen_t address_len = sizeof(address);
  if (getsockname(receiver, (struct sockaddr *)&address, &address_len) < 0) {
    return fail("getsockname");
  }

  int sender = socket(AF_INET, SOCK_DGRAM, 0);
  if (sender < 0) {
    return fail("socket sender");
  }
  const char byte = 'x';
  if (sendto(sender, &byte, sizeof(byte), 0, (struct sockaddr *)&address,
             sizeof(address)) != sizeof(byte)) {
    return fail("sendto");
  }
  char received = 0;
  if (recv(receiver, &received, sizeof(received), 0) != sizeof(received)) {
    return fail("recv");
  }

  int cpu = -1;
  socklen_t cpu_len = sizeof(cpu);
  if (getsockopt(receiver, SOL_SOCKET, SO_INCOMING_CPU, &cpu, &cpu_len) < 0) {
    return fail("getsockopt SO_INCOMING_CPU");
  }
  if (cpu != 0) {
    fprintf(stderr, "expected virtual CPU 0, got %d\n", cpu);
    return 2;
  }
  printf("udp4-incoming-cpu=%d\n", cpu);

  close(sender);
  close(receiver);
  return 0;
}
