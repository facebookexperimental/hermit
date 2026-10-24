/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * The root opens a pipe on descriptors 40 (read) and 41 (write), which no
 * loader read uses, and forks; the child writes "x" to the pipe and exits, and
 * the root reads it, writes "parent\n" and reaps the child.
 * hermit-cli/tests/cli.rs holds the child's write until the root's read has
 * completed, which only that write can satisfy
 * (https://github.com/rrnewton/hermit/issues/3929). In "exit-mid-read" mode a
 * worker thread blocks reading descriptor 40 while the process keeps the
 * write end open, and the main thread writes "main\n" and exits the group, so
 * the worker never completes its read.
 */
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void *reader(void *arg) {
  (void)arg;
  char byte;
  if (read(40, &byte, 1) < 0) {
    return (void *)1;
  }
  return 0;
}

static int exit_mid_read(void) {
  pthread_t thread;
  if (pthread_create(&thread, 0, reader, 0) != 0) {
    return 3;
  }
  struct timespec pause = {0, 1000000};
  nanosleep(&pause, 0);
  if (write(1, "main\n", 5) != 5) {
    return 2;
  }
  exit(0);
}

int main(int argc, char **argv) {
  int fds[2];
  if (pipe(fds) != 0 || dup2(fds[0], 40) != 40 || dup2(fds[1], 41) != 41) {
    return 3;
  }
  close(fds[0]);
  close(fds[1]);
  if (argc > 1 && strcmp(argv[1], "exit-mid-read") == 0) {
    return exit_mid_read();
  }
  pid_t child = fork();
  if (child < 0) {
    return 3;
  }
  if (child == 0) {
    close(40);
    if (write(41, "x", 1) != 1) {
      _exit(2);
    }
    _exit(0);
  }
  close(41);
  char byte;
  if (read(40, &byte, 1) != 1) {
    return 4;
  }
  if (write(1, "parent\n", 7) != 7) {
    return 2;
  }
  waitpid(child, 0, 0);
  return 0;
}
