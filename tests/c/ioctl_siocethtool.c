/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <linux/ethtool.h>
#include <linux/sockios.h>
#include <net/if.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
  int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
  if (fd < 0) {
    perror("socket");
    return EXIT_FAILURE;
  }

  struct ethtool_value value = {
      .cmd = ETHTOOL_GLINK,
  };
  struct ifreq request = {0};
  memcpy(request.ifr_name, "lo", sizeof("lo"));
  request.ifr_data = (void *)&value;

  errno = 0;
  int result = ioctl(fd, SIOCETHTOOL, &request);
  int error = errno;
  if (result != -1 || error != ENODEV) {
    fprintf(stderr, "SIOCETHTOOL returned %d with errno %d, expected ENODEV\n",
            result, error);
    return EXIT_FAILURE;
  }
  if (close(fd) != 0) {
    perror("close");
    return EXIT_FAILURE;
  }

  puts("siocethtool-enodev");
  return EXIT_SUCCESS;
}
