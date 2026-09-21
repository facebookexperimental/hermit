/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

static int copy_fd(int fd) {
  char buffer[4096];
  for (;;) {
    ssize_t count = read(fd, buffer, sizeof(buffer));
    if (count == 0) {
      return 0;
    }
    if (count < 0) {
      return -1;
    }
    for (ssize_t offset = 0; offset < count;) {
      ssize_t written = write(STDOUT_FILENO, buffer + offset, count - offset);
      if (written < 0) {
        return -1;
      }
      offset += written;
    }
  }
}

static int section(const char *name, int fd) {
  if (dprintf(STDOUT_FILENO, "__%s__\n", name) < 0 || copy_fd(fd) < 0 ||
      dprintf(STDOUT_FILENO, "__END_%s__\n", name) < 0) {
    return -1;
  }
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    return 2;
  }

  int mounted = open(argv[1], O_RDONLY);
  int mountinfo = open("/proc/self/mountinfo", O_RDONLY);
  if (mounted < 0 || mountinfo < 0) {
    return 3;
  }

  char fdinfo_path[64];
  if (snprintf(fdinfo_path, sizeof(fdinfo_path), "/proc/self/fdinfo/%d", mounted) < 0) {
    return 4;
  }
  int fdinfo = open(fdinfo_path, O_RDONLY);
  if (fdinfo < 0) {
    return 5;
  }
  int stdout_fdinfo = open("/proc/self/fdinfo/1", O_RDONLY);
  if (stdout_fdinfo < 0) {
    return 6;
  }

  if (section("MOUNTINFO", mountinfo) < 0 || section("FDINFO", fdinfo) < 0 ||
      section("STDOUT_FDINFO", stdout_fdinfo) < 0 ||
      section("MOUNTED", mounted) < 0 || section("STDIN", STDIN_FILENO) < 0) {
    return 7;
  }
  return 0;
}
