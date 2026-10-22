/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A recvmsg whose buffers the `hermit run --verify` buffer digest can or
 * cannot observe after the call.
 *
 * With "plain", iov[0] is {buffer, 16}: the digest hashes buffer+16.
 * With "overwritten", the iovec array is its own receive buffer: iov[0] is
 * {&iov, 16}, and the 16-byte datagram encodes {unmapped page, 1}. Linux
 * imports {&iov, 16} at entry and then writes the payload over it, so after
 * the call the header walk finds an iovec that covers 1 byte of the 16
 * returned, at an unmapped address. The digest records an explicit
 * "unobserved" entry and the guest still receives the kernel's result.
 * Before that entry, the failed read became the guest's recvmsg error
 * (EFAULT) under --verify only.
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

static struct iovec iov[1];
static char buffer[16];

int main(int argc, char** argv) {
  int overwritten = argc > 1 && strcmp(argv[1], "overwritten") == 0;
  void* unmapped =
      mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (unmapped == MAP_FAILED || munmap(unmapped, 4096) != 0) {
    perror("mmap");
    return 1;
  }
  int sockets[2];
  if (socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets) != 0) {
    perror("socketpair");
    return 1;
  }
  struct iovec payload = {unmapped, 1};
  if (send(sockets[0], &payload, sizeof(payload), 0) != sizeof(payload)) {
    perror("send");
    return 1;
  }
  iov[0].iov_base = overwritten ? (void*)iov : (void*)buffer;
  iov[0].iov_len = sizeof(iov[0]);
  struct msghdr message;
  memset(&message, 0, sizeof(message));
  message.msg_iov = iov;
  message.msg_iovlen = 1;
  ssize_t received = recvmsg(sockets[1], &message, 0);
  if (received != sizeof(payload)) {
    perror("recvmsg");
    return 2;
  }
  printf(
      "buffer=%p received=%zd overwritten=%d\n",
      (void*)buffer,
      received,
      iov[0].iov_base == unmapped);
  return 0;
}
