/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static pthread_mutex_t a = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t b = PTHREAD_MUTEX_INITIALIZER;
static pthread_barrier_t held;

static void *thread_one(void *unused) {
  (void)unused;
  pthread_mutex_lock(&a);
  pthread_barrier_wait(&held);
  pthread_mutex_lock(&b);
  return NULL;
}

static void *thread_two(void *unused) {
  (void)unused;
  pthread_mutex_lock(&b);
  pthread_barrier_wait(&held);
  pthread_mutex_lock(&a);
  return NULL;
}

int main(void) {
  pthread_t first;
  pthread_t second;

  pthread_barrier_init(&held, NULL, 3);
  pthread_create(&first, NULL, thread_one, NULL);
  pthread_create(&second, NULL, thread_two, NULL);

  printf("ready\n");
  fflush(stdout);
  char release;
  if (read(STDIN_FILENO, &release, 1) < 0) {
    perror("read");
    return EXIT_FAILURE;
  }

  pthread_barrier_wait(&held);
  pthread_join(first, NULL);
  pthread_join(second, NULL);
  printf("UNREACHABLE: AB-BA deadlock resolved\n");
  return EXIT_SUCCESS;
}
