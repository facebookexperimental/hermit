/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <arpa/inet.h>
#include <linux/sockios.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
  if (argc != 2) {
    fputs("usage: socket_ioctl_timestamp v4-us|v4-ns|v6-us\n", stderr);
    return 1;
  }
  int family = strcmp(argv[1], "v6-us") == 0 ? AF_INET6 : AF_INET;
  int nanoseconds = strcmp(argv[1], "v4-ns") == 0;
  if ((!nanoseconds && strcmp(argv[1], "v4-us") != 0 &&
       strcmp(argv[1], "v6-us") != 0) ||
      (nanoseconds && family != AF_INET)) {
    fputs("unknown mode\n", stderr);
    return 1;
  }

  int receiver = socket(family, SOCK_DGRAM, 0);
  int sender = socket(family, SOCK_DGRAM, 0);
  struct sockaddr_storage address = {0};
  socklen_t address_len;
  if (family == AF_INET) {
    struct sockaddr_in *address4 = (struct sockaddr_in *)&address;
    address4->sin_family = AF_INET;
    address4->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    address_len = sizeof(*address4);
  } else {
    struct sockaddr_in6 *address6 = (struct sockaddr_in6 *)&address;
    address6->sin6_family = AF_INET6;
    address6->sin6_addr = in6addr_loopback;
    address_len = sizeof(*address6);
  }
  if (receiver < 0 || sender < 0 ||
      bind(receiver, (struct sockaddr *)&address, address_len) != 0 ||
      getsockname(receiver, (struct sockaddr *)&address, &address_len) != 0 ||
      sendto(sender, "x", 1, 0, (struct sockaddr *)&address, address_len) !=
          1) {
    perror("setup");
    return 2;
  }

  char byte;
  if (recv(receiver, &byte, 1, 0) != 1) {
    perror("recv");
    return 3;
  }
  if (nanoseconds) {
    struct timespec stamp;
    if (ioctl(receiver, SIOCGSTAMPNS, &stamp) != 0) {
      perror("SIOCGSTAMPNS");
      return 4;
    }
    printf("%ld.%09ld\n", (long)stamp.tv_sec, stamp.tv_nsec);
  } else {
    struct timeval stamp;
    if (ioctl(receiver, SIOCGSTAMP, &stamp) != 0) {
      perror("SIOCGSTAMP");
      return 4;
    }
    printf("%ld.%06ld\n", (long)stamp.tv_sec, (long)stamp.tv_usec);
  }
  return 0;
}
