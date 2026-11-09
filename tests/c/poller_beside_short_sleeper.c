/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A thread blocked in poll() on a pipe that main writes five times, beside a
// thread that sleeps 100 us at a time until the poller has read every byte.
// Under `hermit record` the poll runs in the background, and the sleeper,
// whose sleeps are shorter than one scheduler turn's charge (500 us, fixed
// under record), is runnable at every turn.
//
// Prints "polled 5 of 5 bytes" and exits 0 once the poller has read them all.
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

#define BYTES 5

static int pipe_fds[2];
static atomic_int polled;

static void sleep_us(long us) {
  struct timespec delay = {.tv_sec = 0, .tv_nsec = us * 1000L};
  nanosleep(&delay, NULL);
}

static void* poller(void* arg) {
  (void)arg;
  while (atomic_load(&polled) < BYTES) {
    struct pollfd fd = {.fd = pipe_fds[0], .events = POLLIN};
    char byte;
    if (poll(&fd, 1, -1) == 1 && (fd.revents & POLLIN) &&
        read(pipe_fds[0], &byte, 1) == 1) {
      atomic_fetch_add(&polled, 1);
    }
  }
  return NULL;
}

static void* sleeper(void* arg) {
  (void)arg;
  while (atomic_load(&polled) < BYTES) {
    sleep_us(100);
  }
  return NULL;
}

int main(void) {
  if (pipe(pipe_fds) != 0) {
    perror("pipe");
    return 1;
  }
  pthread_t threads[2];
  if (pthread_create(&threads[0], NULL, sleeper, NULL) != 0 ||
      pthread_create(&threads[1], NULL, poller, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  for (int i = 0; i < BYTES; i++) {
    sleep_us(200);
    if (write(pipe_fds[1], "x", 1) != 1) {
      perror("write");
      return 1;
    }
  }
  pthread_join(threads[1], NULL);
  pthread_join(threads[0], NULL);
  printf("polled %d of %d bytes\n", atomic_load(&polled), BYTES);
  return 0;
}
