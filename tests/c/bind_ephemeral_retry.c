/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Force Detcore's port-0 bind retry: hold the allocator's first port P,
 * put P+1 into TIME_WAIT with a completed loopback connection, then bind
 * port 0. The allocator's next candidate is P+1, which the host still
 * holds in TIME_WAIT, so the bind must move on to another port instead of
 * answering EADDRINUSE - as Linux's ephemeral search does
 * (inet_csk_find_open_port skips conflicting ports). The address to bind
 * is the first argument (a loopback address of the test's own, IPv4 or
 * IPv6, so that P and P+1 are uncontended there). Prints the same line
 * natively and under Hermit: the final bind succeeds on a port other
 * than P and P+1.
 */

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int family = AF_INET;
static const char* address = "127.0.0.1";

static socklen_t make_address(unsigned short port,
                              struct sockaddr_storage* out) {
  memset(out, 0, sizeof(*out));
  if (family == AF_INET6) {
    struct sockaddr_in6* in6 = (struct sockaddr_in6*)out;
    in6->sin6_family = AF_INET6;
    in6->sin6_port = htons(port);
    if (inet_pton(AF_INET6, address, &in6->sin6_addr) != 1) {
      return 0;
    }
    return sizeof(*in6);
  }
  struct sockaddr_in* in = (struct sockaddr_in*)out;
  in->sin_family = AF_INET;
  in->sin_port = htons(port);
  if (inet_pton(AF_INET, address, &in->sin_addr) != 1) {
    return 0;
  }
  return sizeof(*in);
}

static unsigned short bound_port(struct sockaddr_storage* name) {
  if (family == AF_INET6) {
    return ntohs(((struct sockaddr_in6*)name)->sin6_port);
  }
  return ntohs(((struct sockaddr_in*)name)->sin_port);
}

static int bind_port(unsigned short port, unsigned short* bound) {
  int fd = socket(family, SOCK_STREAM, 0);
  if (fd < 0) {
    return -1;
  }
  struct sockaddr_storage name;
  socklen_t length = make_address(port, &name);
  if (length == 0 || bind(fd, (struct sockaddr*)&name, length) != 0) {
    close(fd);
    return -1;
  }
  length = sizeof(name);
  if (getsockname(fd, (struct sockaddr*)&name, &length) != 0) {
    close(fd);
    return -1;
  }
  *bound = bound_port(&name);
  return fd;
}

int main(int argc, char** argv) {
  if (argc > 1) {
    address = argv[1];
  }
  if (strchr(address, ':') != NULL) {
    family = AF_INET6;
  }
  // Up to a few attempts: natively, a neighbour port can be held by an
  // unrelated host socket (or an earlier run's TIME_WAIT), which says
  // nothing about the retry. A failed third bind, below, never retries:
  // that is the bug's signature.
  unsigned short first = 0;
  unsigned short second = 0;
  int held = -1;
  int listener = -1;
  for (int attempt = 0; attempt < 8; attempt++) {
    held = bind_port(0, &first);
    if (held < 0 || first == 0 || first == 65535) {
      printf("retry ok=0 [first bind]\n");
      return 1;
    }
    listener = bind_port((unsigned short)(first + 1), &second);
    if (listener >= 0 && second == first + 1 &&
        listen(listener, 1) == 0) {
      break;
    }
    if (listener >= 0) {
      close(listener);
      listener = -1;
    }
    close(held);
    held = -1;
  }
  if (listener < 0) {
    printf("retry ok=0 [second bind]\n");
    return 1;
  }
  int client = socket(family, SOCK_STREAM, 0);
  struct sockaddr_storage target;
  socklen_t target_length = make_address(second, &target);
  if (client < 0 || target_length == 0 ||
      connect(client, (struct sockaddr*)&target, target_length) != 0) {
    printf("retry ok=0 [connect]\n");
    return 1;
  }
  int accepted = accept(listener, NULL, NULL);
  if (accepted < 0) {
    printf("retry ok=0 [accept]\n");
    return 1;
  }
  close(accepted);
  close(client);
  close(listener);

  unsigned short third = 0;
  errno = 0;
  int next = bind_port(0, &third);
  if (next < 0) {
    printf("retry ok=0 [third bind errno=%d]\n", errno);
    return 1;
  }
  int ok = third != first && third != second;
  // third_minus_first pins the attempt count under a deterministic
  // allocator: exactly one retry lands on first+2 (the held first port
  // and the TIME_WAIT second are both skipped once each).
  printf("retry ok=%d skipped_time_wait=%d third_minus_first=%d\n", ok, ok,
         (int)third - (int)first);
  close(next);
  close(held);
  return ok ? 0 : 1;
}
