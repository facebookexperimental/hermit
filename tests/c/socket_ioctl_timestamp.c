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
  int duplicate = dup(receiver);
  if (duplicate < 0) {
    perror("dup");
    return 4;
  }
  if (nanoseconds) {
    struct timespec stamp;
    struct timespec repeated;
    struct timespec aliased;
    if (ioctl(receiver, SIOCGSTAMPNS, &stamp) != 0 ||
        ioctl(receiver, SIOCGSTAMPNS, &repeated) != 0 ||
        ioctl(duplicate, SIOCGSTAMPNS, &aliased) != 0) {
      perror("SIOCGSTAMPNS");
      return 5;
    }
    if (memcmp(&stamp, &repeated, sizeof(stamp)) != 0 ||
        memcmp(&stamp, &aliased, sizeof(stamp)) != 0) {
      fputs("timestamp changed without another receive\n", stderr);
      return 6;
    }
    usleep(2000);
    if (sendto(sender, "y", 1, 0, (struct sockaddr *)&address, address_len) !=
            1 ||
        recv(receiver, &byte, 1, 0) != 1) {
      perror("second receive");
      return 7;
    }
    struct timespec advanced;
    if (ioctl(receiver, SIOCGSTAMPNS, &advanced) != 0) {
      perror("second SIOCGSTAMPNS");
      return 8;
    }
    if (memcmp(&stamp, &advanced, sizeof(stamp)) == 0) {
      fputs("timestamp did not change after another receive\n", stderr);
      return 9;
    }
    printf("%ld.%09ld\n", (long)stamp.tv_sec, stamp.tv_nsec);
  } else {
    struct timeval stamp;
    struct timeval repeated;
    struct timeval aliased;
    if (ioctl(receiver, SIOCGSTAMP, &stamp) != 0 ||
        ioctl(receiver, SIOCGSTAMP, &repeated) != 0 ||
        ioctl(duplicate, SIOCGSTAMP, &aliased) != 0) {
      perror("SIOCGSTAMP");
      return 5;
    }
    if (memcmp(&stamp, &repeated, sizeof(stamp)) != 0 ||
        memcmp(&stamp, &aliased, sizeof(stamp)) != 0) {
      fputs("timestamp changed without another receive\n", stderr);
      return 6;
    }
    usleep(2000);
    if (sendto(sender, "y", 1, 0, (struct sockaddr *)&address, address_len) !=
            1 ||
        recv(receiver, &byte, 1, 0) != 1) {
      perror("second receive");
      return 7;
    }
    struct timeval advanced;
    if (ioctl(receiver, SIOCGSTAMP, &advanced) != 0) {
      perror("second SIOCGSTAMP");
      return 8;
    }
    if (memcmp(&stamp, &advanced, sizeof(stamp)) == 0) {
      fputs("timestamp did not change after another receive\n", stderr);
      return 9;
    }
    printf("%ld.%06ld\n", (long)stamp.tv_sec, (long)stamp.tv_usec);
  }
  close(duplicate);
  return 0;
}
