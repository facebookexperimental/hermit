/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Opens /proc/uptime, sleeps 5 seconds, then reads it, and prints what it
 * read: Linux renders the file when it is read (uptime_proc_show), so the
 * uptime is the read's, 5 seconds past the open's.
 */

#include <fcntl.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

int main(void) {
  int fd = open("/proc/uptime", O_RDONLY);
  if (fd < 0) {
    perror("open /proc/uptime");
    return 1;
  }
  struct timespec pause = {5, 0};
  if (nanosleep(&pause, NULL) != 0) {
    perror("nanosleep");
    return 1;
  }
  char contents[64];
  ssize_t length = read(fd, contents, sizeof(contents));
  if (length <= 0) {
    perror("read /proc/uptime");
    return 1;
  }
  if (write(STDOUT_FILENO, contents, (size_t)length) != length) {
    return 1;
  }
  return 0;
}
