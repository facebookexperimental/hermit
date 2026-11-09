/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Open the FIFO argv[1] names for reading and writing, write one byte into
// it and read it back. Each write moves the FIFO's modification time.

#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s FIFO\n", argv[0]);
    return 2;
  }
  int fd = open(argv[1], O_RDWR);
  if (fd < 0) {
    perror("open");
    return 1;
  }
  char byte = 'x';
  if (write(fd, &byte, 1) != 1 || read(fd, &byte, 1) != 1) {
    perror("write or read");
    return 1;
  }
  printf("read %c\n", byte);
  return close(fd) == 0 ? 0 : 1;
}
