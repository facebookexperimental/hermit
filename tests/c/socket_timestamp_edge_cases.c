/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#ifndef SO_TIMESTAMPNS
#define SO_TIMESTAMPNS 35
#endif

static int send_byte(int fd, char byte) {
  if (send(fd, &byte, 1, 0) != 1) {
    perror("send");
    return -1;
  }
  return 0;
}

static int check_timespec_message(const struct msghdr *message,
                                  struct timespec *timestamp) {
  struct cmsghdr *header = CMSG_FIRSTHDR(message);
  if (header == NULL || header->cmsg_level != SOL_SOCKET ||
      header->cmsg_type != SO_TIMESTAMPNS ||
      header->cmsg_len < CMSG_LEN(sizeof(*timestamp))) {
    fputs("missing SCM_TIMESTAMPNS\n", stderr);
    return -1;
  }
  memcpy(timestamp, CMSG_DATA(header), sizeof(*timestamp));
  return 0;
}

int main(void) {
  int sockets[2];
  int enabled = 1;
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) != 0 ||
      setsockopt(sockets[1], SOL_SOCKET, SO_TIMESTAMPNS, &enabled,
                 sizeof(enabled)) != 0) {
    perror("setup");
    return 1;
  }

  char byte;
  struct iovec iov = {.iov_base = &byte, .iov_len = sizeof(byte)};

  if (send_byte(sockets[0], 't') != 0) {
    return 2;
  }
  unsigned char truncated[CMSG_SPACE(sizeof(struct timespec))] = {0};
  struct msghdr truncated_message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = truncated,
      .msg_controllen = CMSG_LEN(sizeof(int32_t)),
  };
  if (recvmsg(sockets[1], &truncated_message, 0) != 1) {
    perror("truncated recvmsg");
    return 3;
  }
  if ((truncated_message.msg_flags & MSG_CTRUNC) == 0) {
    fputs("truncated timestamp omitted MSG_CTRUNC\n", stderr);
    return 4;
  }
  struct cmsghdr *truncated_header = CMSG_FIRSTHDR(&truncated_message);
  if (truncated_header == NULL || truncated_header->cmsg_level != SOL_SOCKET ||
      truncated_header->cmsg_type != SO_TIMESTAMPNS) {
    fputs("truncated timestamp omitted its control header\n", stderr);
    return 5;
  }
  int32_t timestamp_prefix;
  memcpy(&timestamp_prefix, CMSG_DATA(truncated_header),
         sizeof(timestamp_prefix));
  if (timestamp_prefix < 1600000000 || timestamp_prefix >= 1704067200) {
    fprintf(stderr,
            "truncated timestamp prefix escaped the fixed logical epoch: %d\n",
            timestamp_prefix);
    return 6;
  }

  if (send_byte(sockets[0], 'a') != 0) {
    return 7;
  }
  union {
    struct msghdr message;
    unsigned char control[CMSG_SPACE(sizeof(struct timespec))];
  } aliased = {0};
  aliased.message.msg_iov = &iov;
  aliased.message.msg_iovlen = 1;
  aliased.message.msg_control = &aliased;
  aliased.message.msg_controllen = sizeof(aliased);
  if (recvmsg(sockets[1], &aliased.message, 0) != 1) {
    perror("aliased recvmsg");
    return 8;
  }

  enum { MESSAGE_COUNT = 2 };
  struct mmsghdr messages[MESSAGE_COUNT] = {0};
  struct iovec iovecs[MESSAGE_COUNT] = {
      {.iov_base = &byte, .iov_len = sizeof(byte)},
      {.iov_base = &byte, .iov_len = sizeof(byte)},
  };
  unsigned char controls[MESSAGE_COUNT]
                        [CMSG_SPACE(sizeof(struct timespec))] = {0};
  for (int index = 0; index < MESSAGE_COUNT; ++index) {
    messages[index].msg_hdr.msg_iov = &iovecs[index];
    messages[index].msg_hdr.msg_iovlen = 1;
    messages[index].msg_hdr.msg_control = controls[index];
    messages[index].msg_hdr.msg_controllen = sizeof(controls[index]);
    if (send_byte(sockets[0], (char)('0' + index)) != 0) {
      return 9;
    }
  }
  if (recvmmsg(sockets[1], messages, MESSAGE_COUNT, 0, NULL) != MESSAGE_COUNT) {
    perror("recvmmsg");
    return 10;
  }
  struct timespec timestamps[MESSAGE_COUNT];
  for (int index = 0; index < MESSAGE_COUNT; ++index) {
    if (check_timespec_message(&messages[index].msg_hdr, &timestamps[index]) !=
        0) {
      return 11;
    }
  }
  if (timestamps[0].tv_sec != timestamp_prefix ||
      timestamps[1].tv_sec != timestamp_prefix) {
    fprintf(stderr,
            "batched timestamps escaped the fixed logical epoch: %ld,%ld != "
            "%d\n",
            (long)timestamps[0].tv_sec, (long)timestamps[1].tv_sec,
            timestamp_prefix);
    return 12;
  }

  printf("truncated=%d alias=ok batch=%ld.%09ld,%ld.%09ld\n",
         timestamp_prefix, (long)timestamps[0].tv_sec,
         timestamps[0].tv_nsec, (long)timestamps[1].tv_sec,
         timestamps[1].tv_nsec);
  return 0;
}
