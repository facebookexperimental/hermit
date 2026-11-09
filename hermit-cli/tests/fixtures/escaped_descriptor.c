/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * Licensed under the BSD-style license in the LICENSE file.
 */

/* An SCM_RIGHTS recipient outside Hermit can wake a guest-created channel.
 * Linux v6.12 scm_fp_copy keeps the original struct file, not a new pipe:
 * https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/core/scm.c?h=v6.12#n108
 * A pipe read waits while a writer exists; a full pipe write waits for room:
 * https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/pipe.c?h=v6.12#n352
 */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <unistd.h>

int main(int argc, char** argv) {
  assert(argc == 3);
  const char* mode = argv[1];
  const int writing = strcmp(mode, "write") == 0 || strcmp(mode, "writev") == 0;
  const int batch = strcmp(mode, "socket") == 0 || strcmp(mode, "writev") == 0;
  int ends[2];
  if (strcmp(mode, "socket") == 0) {
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, ends) == 0);
  } else {
    assert(pipe(ends) == 0);
  }
  if (writing) {
    int flags = fcntl(ends[1], F_GETFL);
    assert(flags >= 0 && fcntl(ends[1], F_SETFL, flags | O_NONBLOCK) == 0);
    char fill[4096];
    memset(fill, 'f', sizeof(fill));
    while (write(ends[1], fill, sizeof(fill)) > 0) {
    }
    assert(errno == EAGAIN);
    assert(fcntl(ends[1], F_SETFL, flags) == 0);
  }

  int channel = socket(AF_UNIX, SOCK_STREAM, 0);
  assert(channel >= 0);
  struct sockaddr_un address = {.sun_family = AF_UNIX};
  assert(strlen(argv[2]) < sizeof(address.sun_path));
  strcpy(address.sun_path, argv[2]);
  assert(connect(channel, (struct sockaddr*)&address, sizeof(address)) == 0);
  int passed = ends[writing ? 0 : 1];
  union {
    struct cmsghdr alignment;
    char bytes[CMSG_SPACE(sizeof(int))];
  } control = {0};
  char byte = 'p';
  struct iovec vector = {.iov_base = &byte, .iov_len = 1};
  struct mmsghdr messages[2] = {0};
  messages[0].msg_hdr.msg_iov = &vector;
  messages[0].msg_hdr.msg_iovlen = 1;
  messages[1] = messages[0];
  struct msghdr* message = &messages[batch ? 1 : 0].msg_hdr;
  message->msg_control = control.bytes;
  message->msg_controllen = sizeof(control.bytes);
  struct cmsghdr* header = CMSG_FIRSTHDR(message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &passed, sizeof(passed));
  if (batch) {
    assert(sendmmsg(channel, messages, 2, 0) == 2);
  } else {
    assert(sendmsg(channel, message, 0) == 1);
  }
  assert(close(channel) == 0);
  /* Keep our peer open too: EOF/EPIPE must not substitute for the host's wake. */
  if (writing) {
    byte = 'x';
    vector.iov_base = &byte;
    vector.iov_len = 1;
    ssize_t count = strcmp(mode, "writev") == 0 ? writev(ends[1], &vector, 1)
                                                : write(ends[1], &byte, 1);
    assert(count == 1);
  } else {
    assert(read(ends[0], &byte, 1) == 1 && byte == 'x');
  }
  printf("RESULT %s\n", mode);
  return 0;
}
