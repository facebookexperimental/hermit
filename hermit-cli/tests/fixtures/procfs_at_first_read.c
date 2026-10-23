/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Opens a procfs file (by default /proc/uptime, else the one named), sleeps
 * 5 seconds, then reads it, and prints what it read: Linux renders
 * /proc/uptime and /proc/stat when they are read (uptime_proc_show,
 * show_stat), so the time they show is the read's, 5 seconds past the
 * open's.
 */

#include <fcntl.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char** argv) {
  const char* path = argc > 1 ? argv[1] : "/proc/uptime";
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    perror(path);
    return 1;
  }
  struct timespec pause = {5, 0};
  if (nanosleep(&pause, NULL) != 0) {
    perror("nanosleep");
    return 1;
  }
  static char contents[1 << 20];
  ssize_t length = read(fd, contents, sizeof(contents));
  if (length <= 0) {
    perror(path);
    return 1;
  }
  if (write(STDOUT_FILENO, contents, (size_t)length) != length) {
    return 1;
  }
  return 0;
}
