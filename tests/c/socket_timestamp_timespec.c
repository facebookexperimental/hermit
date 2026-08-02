/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#ifndef SO_TIMESTAMPNS
#define SO_TIMESTAMPNS 35
#endif

int main(void) {
  int sockets[2];
  int enabled = 1;
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) != 0 ||
      setsockopt(sockets[1], SOL_SOCKET, SO_TIMESTAMPNS, &enabled,
                 sizeof(enabled)) != 0 ||
      send(sockets[0], "x", 1, 0) != 1) {
    perror("setup");
    return 1;
  }

  char byte;
  char control[CMSG_SPACE(sizeof(struct timespec))] = {0};
  struct iovec iov = {.iov_base = &byte, .iov_len = sizeof(byte)};
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control,
      .msg_controllen = sizeof(control),
  };
  if (recvmsg(sockets[1], &message, 0) != 1) {
    perror("recvmsg");
    return 2;
  }
  struct cmsghdr *header = CMSG_FIRSTHDR(&message);
  if (header == NULL || header->cmsg_level != SOL_SOCKET ||
      header->cmsg_type != SO_TIMESTAMPNS ||
      header->cmsg_len < CMSG_LEN(sizeof(struct timespec))) {
    fputs("missing SCM_TIMESTAMPNS\n", stderr);
    return 3;
  }
  struct timespec value;
  memcpy(&value, CMSG_DATA(header), sizeof(value));
  struct timespec observed_now;
  if (clock_gettime(CLOCK_REALTIME, &observed_now) != 0) {
    perror("clock_gettime");
    return 4;
  }
  if (value.tv_sec < 0 || value.tv_nsec < 0 || value.tv_nsec >= 1000000000L ||
      value.tv_sec > observed_now.tv_sec ||
      observed_now.tv_sec - value.tv_sec > 1 ||
      (value.tv_sec == observed_now.tv_sec &&
       value.tv_nsec > observed_now.tv_nsec)) {
    fprintf(stderr,
            "SCM_TIMESTAMPNS escaped logical time: timestamp=%ld.%09ld "
            "now=%ld.%09ld\n",
            (long)value.tv_sec, value.tv_nsec, (long)observed_now.tv_sec,
            observed_now.tv_nsec);
    return 5;
  }
  puts("timestampns=ok");
  return 0;
}
