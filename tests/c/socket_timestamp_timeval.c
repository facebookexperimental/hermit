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
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

int main(void) {
  int receiver = socket(AF_INET, SOCK_DGRAM, 0);
  int sender = socket(AF_INET, SOCK_DGRAM, 0);
  struct sockaddr_in address = {
      .sin_family = AF_INET,
      .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
  };
  socklen_t address_len = sizeof(address);
  int enabled = 1;
  if (receiver < 0 || sender < 0 ||
      bind(receiver, (struct sockaddr *)&address, sizeof(address)) != 0 ||
      getsockname(receiver, (struct sockaddr *)&address, &address_len) != 0 ||
      setsockopt(receiver, SOL_SOCKET, SO_TIMESTAMP, &enabled,
                 sizeof(enabled)) != 0 ||
      sendto(sender, "x", 1, 0, (struct sockaddr *)&address, sizeof(address)) !=
          1) {
    perror("setup");
    return 1;
  }

  char byte;
  char control[CMSG_SPACE(sizeof(struct timeval))] = {0};
  struct iovec iov = {.iov_base = &byte, .iov_len = sizeof(byte)};
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  if (recvmsg(receiver, &message, 0) != 1) {
    perror("recvmsg");
    return 2;
  }
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_level != SOL_SOCKET ||
      header->cmsg_type != SCM_TIMESTAMP) {
    fputs("missing SCM_TIMESTAMP\n", stderr);
    return 3;
  }
  struct timeval value;
  memcpy(&value, CMSG_DATA(header), sizeof(value));
  printf("%ld.%06ld\n", (long)value.tv_sec, (long)value.tv_usec);
  return 0;
}
