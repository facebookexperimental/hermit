/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A ping-pong over a Unix stream socketpair beside a thread that spins
 * without system calls.
 *
 * usage: socket_reader_beside_spinner ROUNDS DEADLINE_S
 *
 * A reader thread waits in poll(-1) on one end of the socketpair, reads a
 * byte and writes it back. A client thread, ROUNDS times, writes a byte to
 * the other end, waits in poll(-1) for the answer and reads it. Main spins,
 * reading CLOCK_MONOTONIC once per 2^20 iterations of a loop that makes no
 * system call, until the client is done or DEADLINE_S seconds have passed.
 * Prints how many rounds finished and when, and exits 0 if all did, 1 if not.
 */

#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static int sv[2];
static int rounds;
static atomic_int done;
static atomic_int finished;

static long long now_us(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void wait_readable(int fd) {
  struct pollfd p = {.fd = fd, .events = POLLIN, .revents = 0};
  if (poll(&p, 1, -1) != 1 || !(p.revents & POLLIN)) {
    fprintf(stderr, "poll failed\n");
    exit(2);
  }
}

static void* reader(void* arg) {
  (void)arg;
  for (;;) {
    char byte;
    wait_readable(sv[1]);
    if (read(sv[1], &byte, 1) != 1 || write(sv[1], &byte, 1) != 1) {
      return NULL;
    }
  }
}

static void* client(void* arg) {
  (void)arg;
  for (int i = 0; i < rounds; i++) {
    char byte = (char)i;
    if (write(sv[0], &byte, 1) != 1) {
      exit(2);
    }
    wait_readable(sv[0]);
    if (read(sv[0], &byte, 1) != 1) {
      exit(2);
    }
    atomic_fetch_add(&finished, 1);
  }
  atomic_store(&done, 1);
  return NULL;
}

int main(int argc, char** argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s ROUNDS DEADLINE_S\n", argv[0]);
    return 2;
  }
  rounds = atoi(argv[1]);
  long long deadline_us = atoll(argv[2]) * 1000000;
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
    return 2;
  }
  pthread_t threads[2];
  if (pthread_create(&threads[0], NULL, reader, NULL) != 0 ||
      pthread_create(&threads[1], NULL, client, NULL) != 0) {
    return 2;
  }
  long long start = now_us();
  long long elapsed = 0;
  volatile unsigned long spins = 0;
  while (!atomic_load(&done) && elapsed < deadline_us) {
    for (unsigned long i = 0; i < (1ul << 20); i++) {
      spins = spins + 1;
    }
    elapsed = now_us() - start;
  }
  int all = atomic_load(&done);
  printf("%d/%d rounds by %lld us\n", atomic_load(&finished), rounds, elapsed);
  fflush(stdout);
  _exit(all ? 0 : 1);
}
