/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A receive whose output overwrites its own iovec. Linux imports the original
 * geometry before copying the datagram. A certified observer must hash the
 * received payload, not temporarily reinstated metadata. The independently
 * sent bytes are printed so the test can calculate the exact expected hash.
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
  void* destination = overwritten ? (void*)iov : (void*)buffer;
  if (memcmp(destination, &payload, sizeof(payload)) != 0) {
    fputs("received payload differs from the sent bytes\n", stderr);
    return 3;
  }
  printf(
      "buffer=%p received=%zd overwritten=%d\n",
      (void*)buffer,
      received,
      iov[0].iov_base == unmapped);
  printf("destination=%p sent=", destination);
  for (size_t i = 0; i < sizeof(payload); ++i) {
    printf("%02x", ((const unsigned char*)&payload)[i]);
  }
  putchar('\n');
  return 0;
}
