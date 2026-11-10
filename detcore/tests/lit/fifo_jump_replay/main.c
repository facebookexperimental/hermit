/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The main thread opens a FIFO it made for reading while its only peer, a
// thread asleep for 20 ms, will open the writing end. Nothing else can run
// before the sleeper's deadline, so the scheduler moves global time there,
// and records that jump. A replay must make it at the same point: after the
// open has started polling, not as soon as the replay reaches the jump's
// place in the stream, which turns that store no record precede.
//
// Use a bogus 'RUN' directive so this test isn't disabled:
// RUN: true

#include <assert.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static const char* path = "fifo";

static void stamp(const char* what) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  printf("%s %ld.%09ld\n", what, (long)now.tv_sec, now.tv_nsec);
}

static void* writer(void* arg) {
  (void)arg;
  struct timespec pause = {0, 20000000};
  nanosleep(&pause, NULL);
  int fd = open(path, O_WRONLY);
  assert(fd >= 0);
  assert(write(fd, "x", 1) == 1);
  assert(close(fd) == 0);
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  unlink(path);
  assert(mkfifo(path, 0600) == 0);
  pthread_t thread;
  assert(pthread_create(&thread, NULL, writer, NULL) == 0);
  int fd = open(path, O_RDONLY);
  assert(fd >= 0);
  char byte;
  assert(read(fd, &byte, 1) == 1);
  stamp("read");
  assert(pthread_join(thread, NULL) == 0);
  stamp("end");
  return 0;
}
