/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* A child that leaves the run's process group, by `setsid` or by
 * `setpgid(0, 0)`, and keeps running after the root exits: it appends a byte
 * to the file named by argv[2] every millisecond, 200 times. A backend that
 * contained the run by its process group would leave the child running after
 * Hermit returns, and the file would keep growing; the caller checks that it
 * does not (slice B3a-4c of https://github.com/rrnewton/hermit/issues/4266). */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char** argv) {
  if (argc != 3 ||
      (strcmp(argv[1], "setsid") != 0 && strcmp(argv[1], "setpgid") != 0)) {
    fprintf(stderr, "usage: %s setsid|setpgid MARKER\n", argv[0]);
    return 2;
  }
  int ready[2];
  if (pipe(ready) != 0) {
    perror("pipe");
    return 2;
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 2;
  }
  if (child == 0) {
    close(ready[0]);
    int left = strcmp(argv[1], "setsid") == 0 ? (int)setsid() : setpgid(0, 0);
    char outcome = left < 0 ? 'E' : 'L';
    (void)!write(ready[1], &outcome, 1);
    close(ready[1]);
    int marker = open(argv[2], O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644);
    if (marker < 0)
      _exit(1);
    struct timespec millisecond = {0, 1000000};
    for (int i = 0; i < 200; i++) {
      (void)!write(marker, "x", 1);
      nanosleep(&millisecond, NULL);
    }
    _exit(0);
  }
  close(ready[1]);
  char outcome = 0;
  if (read(ready[0], &outcome, 1) != 1) {
    perror("read");
    return 2;
  }
  printf("%s %s\n", argv[1], outcome == 'L' ? "left the group" : "failed");
  return 0;
}
