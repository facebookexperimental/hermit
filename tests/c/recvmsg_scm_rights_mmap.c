/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

static void fail(const char *message) {
  perror(message);
  exit(EXIT_FAILURE);
}

int main(void) {
  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) != 0) {
    fail("socketpair");
  }

  int source = open("/bin/sh", O_RDONLY);
  if (source < 0) {
    fail("open");
  }

  char byte = 'x';
  struct iovec send_iov = {.iov_base = &byte, .iov_len = sizeof(byte)};
  char send_control[CMSG_SPACE(sizeof(source))] = {0};
  struct msghdr send_message = {
      .msg_iov = &send_iov,
      .msg_iovlen = 1,
      .msg_control = send_control,
      .msg_controllen = sizeof(send_control),
  };
  struct cmsghdr *send_cmsg = CMSG_FIRSTHDR(&send_message);
  send_cmsg->cmsg_level = SOL_SOCKET;
  send_cmsg->cmsg_type = SCM_RIGHTS;
  send_cmsg->cmsg_len = CMSG_LEN(sizeof(source));
  memcpy(CMSG_DATA(send_cmsg), &source, sizeof(source));

  if (sendmsg(sockets[0], &send_message, 0) != sizeof(byte)) {
    fail("sendmsg");
  }

  char received_byte = 0;
  struct iovec receive_iov = {
      .iov_base = &received_byte,
      .iov_len = sizeof(received_byte),
  };
  char receive_control[CMSG_SPACE(sizeof(source))] = {0};
  struct msghdr receive_message = {
      .msg_iov = &receive_iov,
      .msg_iovlen = 1,
      .msg_control = receive_control,
      .msg_controllen = sizeof(receive_control),
  };

  if (recvmsg(sockets[1], &receive_message, 0) != sizeof(received_byte)) {
    fail("recvmsg");
  }
  struct cmsghdr *receive_cmsg = CMSG_FIRSTHDR(&receive_message);
  if (received_byte != byte || receive_cmsg == NULL ||
      receive_cmsg->cmsg_level != SOL_SOCKET ||
      receive_cmsg->cmsg_type != SCM_RIGHTS ||
      receive_cmsg->cmsg_len != CMSG_LEN(sizeof(source))) {
    fputs("invalid recvmsg output\n", stderr);
    return EXIT_FAILURE;
  }

  int received;
  memcpy(&received, CMSG_DATA(receive_cmsg), sizeof(received));
  unsigned char *mapping =
      mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, received, 0);
  if (mapping == MAP_FAILED) {
    fail("mmap");
  }
  if (memcmp(mapping, "\x7f"
                      "ELF",
             4) != 0) {
    fputs("mapped descriptor did not contain an ELF file\n", stderr);
    return EXIT_FAILURE;
  }

  if (munmap(mapping, 4096) != 0) {
    fail("munmap");
  }
  if (close(received) != 0 || close(source) != 0 || close(sockets[0]) != 0 ||
      close(sockets[1]) != 0) {
    fail("close");
  }

  puts("recvmsg-scm-rights-mmap-ok");
  return EXIT_SUCCESS;
}
