/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Read /proc/self/mountinfo N times (argv[1], default 3000) and print the
// fewest and most lines any read saw. Go reads mountinfo at every process
// start, so a long build rereads it constantly
// (https://github.com/rrnewton/hermit/issues/4167).
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char** argv) {
  int n = argc > 1 ? atoi(argv[1]) : 3000;
  static char buf[1 << 16];
  int fewest = -1, most = 0;
  for (int i = 0; i < n; i++) {
    int fd = open("/proc/self/mountinfo", O_RDONLY);
    if (fd < 0) {
      perror("open");
      return 1;
    }
    int lines = 0;
    ssize_t r;
    while ((r = read(fd, buf, sizeof(buf))) > 0) {
      for (ssize_t k = 0; k < r; k++) {
        lines += buf[k] == '\n';
      }
    }
    if (r < 0) {
      perror("read");
      return 1;
    }
    close(fd);
    if (fewest < 0 || lines < fewest) {
      fewest = lines;
    }
    if (lines > most) {
      most = lines;
    }
  }
  printf("%d reads; lines %s\n", n, fewest == most ? "constant" : "changed");
  return 0;
}
