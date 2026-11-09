/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Say it is running, then wait for one line on stdin before exiting, so that
// a test can change the host's files between the exec and the run's end.

#include <stdio.h>
#include <unistd.h>

int main(void) {
  printf("ready\n");
  fflush(stdout);
  char line[16];
  ssize_t count = read(0, line, sizeof line);
  printf("read %zd\n", count);
  return count > 0 ? 0 : 1;
}
