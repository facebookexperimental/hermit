/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved. Licensed under the BSD-style license in LICENSE. */

#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* No supervisor, stdout marker, or post-exec observation detects a missing
 * replacement here: an unbound survivor must fail in the runtime itself. */
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t parked = PTHREAD_COND_INITIALIZER;
static const char* program;

static void require(int success) {
  if (!success)
    _exit(90);
}

static void* worker(void* unused) {
  (void)unused;
  /* Main held this mutex before pthread_create and only releases it in the
   * condition wait. Keep it locked until exec, including any spurious wake. */
  require(pthread_mutex_lock(&mutex) == 0);
  execl(program, program, "replacement", (char*)NULL);
  _exit(91);
}

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "replacement") == 0)
    _exit(0);
  require(argc == 1);
  program = argv[0];
  require(pthread_mutex_lock(&mutex) == 0);
  pthread_t thread;
  require(pthread_create(&thread, NULL, worker, NULL) == 0);
  for (;;)
    require(pthread_cond_wait(&parked, &mutex) == 0);
}
