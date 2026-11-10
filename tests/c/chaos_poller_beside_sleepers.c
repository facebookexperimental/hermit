/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A thread blocked in poll(2) on a pipe must see a later write to it, however
 * busy other threads keep the scheduler.
 *
 * The server thread waits in poll(2), with no timeout, for a byte on a pipe,
 * reads it and posts a semaphore. Two threads sleep in 1 ms loops. Main, once
 * per round, sleeps WAIT_MS milliseconds in one call, writes one byte to the
 * pipe and waits on the semaphore for up to REPLY_DEADLINE_S seconds of
 * CLOCK_REALTIME. It does that ROUNDS times. argv[1] and argv[2] override
 * ROUNDS and WAIT_MS.
 *
 * On Linux every round is answered at once: pipe_write wakes the poller, and
 * the sleepers block almost all the time. Prints "<rounds> rounds answered"
 * and exits 0, or names the first round the server missed and exits 1.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 20
#define WAIT_MS 20
#define REPLY_DEADLINE_S 5

static int request_pipe[2];
static sem_t answered;
static atomic_int stop_sleepers;

static void sleep_ms(long ms) {
  struct timespec ts = {.tv_sec = ms / 1000, .tv_nsec = (ms % 1000) * 1000000};
  while (clock_nanosleep(CLOCK_MONOTONIC, 0, &ts, &ts) == EINTR) {
  }
}

static void* sleeper(void* arg) {
  (void)arg;
  while (!atomic_load(&stop_sleepers)) {
    sleep_ms(1);
  }
  return NULL;
}

static void* server(void* arg) {
  (void)arg;
  for (;;) {
    struct pollfd pfd = {.fd = request_pipe[0], .events = POLLIN};
    if (poll(&pfd, 1, -1) < 0) {
      if (errno == EINTR) {
        continue;
      }
      perror("poll");
      exit(2);
    }
    char byte;
    if (read(request_pipe[0], &byte, 1) != 1) {
      perror("read");
      exit(2);
    }
    if (byte == 'q') {
      return NULL;
    }
    sem_post(&answered);
  }
}

int main(int argc, char** argv) {
  int rounds = argc > 1 ? atoi(argv[1]) : ROUNDS;
  long wait_ms = argc > 2 ? atol(argv[2]) : WAIT_MS;
  if (pipe(request_pipe) != 0 || sem_init(&answered, 0, 0) != 0) {
    perror("setup");
    return 2;
  }
  pthread_t server_thread;
  pthread_t sleepers[2];
  if (pthread_create(&server_thread, NULL, server, NULL) != 0) {
    return 2;
  }
  for (int i = 0; i < 2; i++) {
    if (pthread_create(&sleepers[i], NULL, sleeper, NULL) != 0) {
      return 2;
    }
  }
  for (int round = 0; round < rounds; round++) {
    sleep_ms(wait_ms);
    if (write(request_pipe[1], "r", 1) != 1) {
      perror("write");
      return 2;
    }
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_sec += REPLY_DEADLINE_S;
    int waited;
    while ((waited = sem_timedwait(&answered, &deadline)) != 0 &&
           errno == EINTR) {
    }
    if (waited != 0) {
      printf(
          "round %d: the poller did not see the write within %d s\n",
          round,
          REPLY_DEADLINE_S);
      fflush(stdout);
      _exit(1);
    }
  }
  if (write(request_pipe[1], "q", 1) != 1) {
    perror("write");
    return 2;
  }
  pthread_join(server_thread, NULL);
  atomic_store(&stop_sleepers, 1);
  for (int i = 0; i < 2; i++) {
    pthread_join(sleepers[i], NULL);
  }
  printf("%d rounds answered\n", rounds);
  return 0;
}
