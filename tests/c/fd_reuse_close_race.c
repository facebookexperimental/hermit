/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A deferred second close of a descriptor number another thread reopened:
// the pattern Android's fdsan reports as a double close.
//
// The closer opens its own descriptor, closes it and tells the writer
// through a pipe. The writer opens a file, which takes the freed number,
// tells the closer through a second pipe, works for a while and then seeks
// the file. The closer, once told, closes the same number again. When that
// second close comes before the writer's seek, it closes the writer's file
// and the seek fails with EBADF: the guest prints a marker to stderr and
// exits 1. When the seek comes first the guest passes. The pipes order the
// first close before the open and the open before the second close in
// every schedule; nothing orders the second close against the seek. Without
// chaos the writer keeps running through its work to the seek and the guest
// passes; with --max-timeslice=400000, chaos preempts the work and every one
// of seeds 1-20 failed.

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static volatile long sink;
// The closer writes a byte here after its first close.
static int first_close_done[2];
// The writer writes a byte here after its open.
static int reopened[2];

static void* closer(void* arg) {
  (void)arg;
  char byte;
  int fd = open("/dev/null", O_RDONLY);
  if (fd < 0) {
    perror("open");
    exit(2);
  }
  close(fd);
  if (write(first_close_done[1], "x", 1) != 1 ||
      read(reopened[0], &byte, 1) != 1) {
    perror("pipe");
    exit(2);
  }
  // Deferred second close: it closes whatever holds the number now.
  close(fd);
  return NULL;
}

static void* writer(void* arg) {
  (void)arg;
  char byte;
  if (read(first_close_done[0], &byte, 1) != 1) {
    perror("read");
    exit(2);
  }
  int fd = open("/dev/null", O_WRONLY);
  if (fd < 0) {
    perror("open");
    exit(2);
  }
  if (write(reopened[1], "x", 1) != 1) {
    perror("write");
    exit(2);
  }
  for (long i = 0; i < 3000000; i++) {
    sink += i;
  }
  if (lseek(fd, 0, SEEK_SET) < 0 && errno == EBADF) {
    fprintf(
        stderr,
        "fd_reuse_close_race: lseek(%d) failed with EBADF: another thread "
        "closed the number\n",
        fd);
    exit(1);
  }
  close(fd);
  return NULL;
}

int main(void) {
  if (pipe(first_close_done) != 0 || pipe(reopened) != 0) {
    perror("pipe");
    return 2;
  }
  pthread_t threads[2];
  if (pthread_create(&threads[0], NULL, closer, NULL) != 0 ||
      pthread_create(&threads[1], NULL, writer, NULL) != 0) {
    fprintf(stderr, "fd_reuse_close_race: pthread_create failed\n");
    return 2;
  }
  pthread_join(threads[0], NULL);
  pthread_join(threads[1], NULL);
  printf("fd_reuse_close_race: no descriptor was closed under the writer\n");
  return 0;
}
