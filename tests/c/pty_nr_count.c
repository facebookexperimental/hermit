/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

static int pty_count(void) {
  FILE* file = fopen("/proc/sys/kernel/pty/nr", "r");
  int count = -1;
  if (file == NULL || fscanf(file, "%d", &count) != 1) {
    return -1;
  }
  fclose(file);
  return count;
}

int main(void) {
  if (pty_count() != 0) {
    return 1;
  }
  int master = open("/dev/ptmx", O_RDWR | O_NOCTTY);
  if (master < 0 || pty_count() != 1) {
    return 1;
  }
  int alias = dup(master);
  if (alias < 0 || pty_count() != 1) {
    return 1;
  }
  close(master);
  if (pty_count() != 1) {
    return 1;
  }
  close(alias);
  if (pty_count() != 0) {
    return 1;
  }
  puts("pty-count-tracks-open-files-ok");
  return 0;
}
