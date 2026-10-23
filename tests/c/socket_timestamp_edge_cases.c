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

static int check_timespec_message(
    const struct msghdr* message,
    struct timespec* timestamp) {
  struct cmsghdr* header = CMSG_FIRSTHDR(message);
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
      setsockopt(
          sockets[1], SOL_SOCKET, SO_TIMESTAMPNS, &enabled, sizeof(enabled)) !=
          0) {
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
  struct cmsghdr* truncated_header = CMSG_FIRSTHDR(&truncated_message);
  if (truncated_header == NULL || truncated_header->cmsg_level != SOL_SOCKET ||
      truncated_header->cmsg_type != SO_TIMESTAMPNS) {
    fputs("truncated timestamp omitted its control header\n", stderr);
    return 5;
  }
  int32_t timestamp_prefix;
  memcpy(
      &timestamp_prefix, CMSG_DATA(truncated_header), sizeof(timestamp_prefix));
  struct timespec observed_now;
  if (clock_gettime(CLOCK_REALTIME, &observed_now) != 0) {
    perror("clock_gettime");
    return 6;
  }
  if (timestamp_prefix < 0 || (time_t)timestamp_prefix > observed_now.tv_sec ||
      observed_now.tv_sec - (time_t)timestamp_prefix > 1) {
    fprintf(
        stderr,
        "truncated timestamp prefix escaped logical time: "
        "timestamp=%d now=%ld\n",
        timestamp_prefix,
        (long)observed_now.tv_sec);
    return 7;
  }

  if (send_byte(sockets[0], 'a') != 0) {
    return 8;
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
    return 9;
  }

  enum { MESSAGE_COUNT = 2 };
  struct mmsghdr messages[MESSAGE_COUNT] = {0};
  struct iovec iovecs[MESSAGE_COUNT] = {
      {.iov_base = &byte, .iov_len = sizeof(byte)},
      {.iov_base = &byte, .iov_len = sizeof(byte)},
  };
  unsigned char controls[MESSAGE_COUNT][CMSG_SPACE(sizeof(struct timespec))] = {
      0};
  for (int index = 0; index < MESSAGE_COUNT; ++index) {
    messages[index].msg_hdr.msg_iov = &iovecs[index];
    messages[index].msg_hdr.msg_iovlen = 1;
    messages[index].msg_hdr.msg_control = controls[index];
    messages[index].msg_hdr.msg_controllen = sizeof(controls[index]);
    if (send_byte(sockets[0], (char)('0' + index)) != 0) {
      return 10;
    }
  }
  if (recvmmsg(sockets[1], messages, MESSAGE_COUNT, 0, NULL) != MESSAGE_COUNT) {
    perror("recvmmsg");
    return 11;
  }
  struct timespec timestamps[MESSAGE_COUNT];
  for (int index = 0; index < MESSAGE_COUNT; ++index) {
    if (check_timespec_message(&messages[index].msg_hdr, &timestamps[index]) !=
        0) {
      return 12;
    }
  }
  /* Each batched timestamp must lie between the clock read before the bytes
   * were sent and one taken after they were received: that holds for logical
   * time whatever the epoch and the clock's rate, and natively for packet
   * times. Requiring the whole second of the first timestamp failed whenever
   * a second boundary fell between the two receives, which depends on the
   * epoch's fraction: a window of about 3 ms with the preemption timer, and
   * about 108 ms without it, where virtual time runs 500 times faster. */
  struct timespec received_now;
  if (clock_gettime(CLOCK_REALTIME, &received_now) != 0) {
    perror("clock_gettime");
    return 6;
  }
  for (int index = 0; index < MESSAGE_COUNT; ++index) {
    const struct timespec* stamp = &timestamps[index];
    int before_sends = stamp->tv_sec < observed_now.tv_sec ||
        (stamp->tv_sec == observed_now.tv_sec &&
         stamp->tv_nsec < observed_now.tv_nsec);
    int after_receive = stamp->tv_sec > received_now.tv_sec ||
        (stamp->tv_sec == received_now.tv_sec &&
         stamp->tv_nsec > received_now.tv_nsec);
    if (before_sends || after_receive) {
      fprintf(
          stderr,
          "batched timestamp %d escaped logical time: %ld.%09ld outside "
          "[%ld.%09ld, %ld.%09ld]\n",
          index,
          (long)stamp->tv_sec,
          stamp->tv_nsec,
          (long)observed_now.tv_sec,
          observed_now.tv_nsec,
          (long)received_now.tv_sec,
          received_now.tv_nsec);
      return 13;
    }
  }

  puts("truncated=ok alias=ok batch=ok");
  return 0;
}
