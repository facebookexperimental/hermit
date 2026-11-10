/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Opens three kinds of file for writing and writes a byte to each: a regular
// file in the run's own /tmp, /dev/null (a character device) and a new
// pipe's write end. An access trace's open notes name each one's path and
// kind.

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static void must(int ok, const char* what) {
  if (!ok) {
    perror(what);
    exit(1);
  }
}

int main(void) {
  char path[] = "/tmp/access_trace_open_kinds.XXXXXX";
  int file = mkstemp(path);
  must(file >= 0, "mkstemp");
  must(unlink(path) == 0, "unlink");
  int null = open("/dev/null", O_WRONLY);
  must(null >= 0, "open /dev/null");
  int ends[2];
  must(pipe2(ends, 0) == 0, "pipe2");
  must(write(file, "f", 1) == 1, "write file");
  must(write(null, "n", 1) == 1, "write /dev/null");
  must(write(ends[1], "p", 1) == 1, "write pipe");
  return 0;
}
