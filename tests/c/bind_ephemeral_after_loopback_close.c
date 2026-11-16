/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A port-0 bind after a completed loopback connection has closed: Linux's
 * ephemeral search skips the closed connection's TIME_WAIT port and binds
 * another free port. Detcore assigns deterministic ephemeral ports itself,
 * so it must likewise move to the next candidate instead of re-offering
 * the just-released port, whose host TIME_WAIT makes the bind fail
 * EADDRINUSE. The printed line is the same natively and under Hermit:
 * the second bind succeeds, on a different port, while the TIME_WAIT
 * (60 s on Linux) outlasts this process.
 */

#include <arpa/inet.h>
#include <string.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int family = AF_INET;
static const char* loopback = "127.0.0.1";

static socklen_t make_address(unsigned short port,
                              struct sockaddr_storage* out) {
  memset(out, 0, sizeof(*out));
  if (family == AF_INET6) {
    struct sockaddr_in6* in6 = (struct sockaddr_in6*)out;
    in6->sin6_family = AF_INET6;
    in6->sin6_port = htons(port);
    if (inet_pton(AF_INET6, loopback, &in6->sin6_addr) != 1) {
      return 0;
    }
    return sizeof(*in6);
  }
  struct sockaddr_in* in = (struct sockaddr_in*)out;
  in->sin_family = AF_INET;
  in->sin_port = htons(port);
  if (inet_pton(AF_INET, loopback, &in->sin_addr) != 1) {
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

static int loopback_listener(unsigned* port) {
  int fd = socket(family, SOCK_STREAM, 0);
  if (fd < 0) {
    return -1;
  }
  struct sockaddr_storage address;
  socklen_t length = make_address(0, &address);
  if (length == 0 || bind(fd, (struct sockaddr*)&address, length) != 0) {
    close(fd);
    return -1;
  }
  length = sizeof(address);
  if (getsockname(fd, (struct sockaddr*)&address, &length) != 0) {
    close(fd);
    return -1;
  }
  *port = bound_port(&address);
  return fd;
}

int main(int argc, char** argv) {
  if (argc > 1) {
    loopback = argv[1];
  }
  if (strchr(loopback, ':') != NULL) {
    family = AF_INET6;
  }
  unsigned listener_port = 0;
  int listener = loopback_listener(&listener_port);
  if (listener < 0 || listen(listener, 1) != 0) {
    printf("rebind ok=0 [first listener]\n");
    return 1;
  }
  int client = socket(family, SOCK_STREAM, 0);
  struct sockaddr_storage target;
  socklen_t target_length =
      make_address((unsigned short)listener_port, &target);
  if (client < 0 || target_length == 0 ||
      connect(client, (struct sockaddr*)&target, target_length) != 0) {
    printf("rebind ok=0 [connect]\n");
    return 1;
  }
  int accepted = accept(listener, NULL, NULL);
  if (accepted < 0) {
    printf("rebind ok=0 [accept]\n");
    return 1;
  }
  // The close order a connected pair's teardown uses: the accepted side
  // first, then the client, then the listener.
  close(accepted);
  close(client);
  close(listener);

  unsigned second_port = 0;
  errno = 0;
  int second = loopback_listener(&second_port);
  if (second < 0) {
    printf("rebind ok=0 [second bind errno=%d]\n", errno);
    return 1;
  }
  close(second);
  printf("rebind ok=1 port_differs=%d\n", second_port != listener_port);
  return second_port != listener_port ? 0 : 1;
}
