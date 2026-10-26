// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

// A thread that never arms its hardware watchpoints. The main thread blocks
// SIGTRAP before it starts a worker; glibc's start code gives the worker the
// creator's mask, so the worker blocks SIGTRAP at every stop it makes and
// `hermit run --watch` cannot arm it. The worker writes the watched global.

#include <pthread.h>
#include <signal.h>
#include <stdio.h>

volatile long watched;

static void* worker(void* arg) {
  (void)arg;
  watched = 2;
  return NULL;
}

int main(void) {
  sigset_t trap;
  sigemptyset(&trap);
  sigaddset(&trap, SIGTRAP);
  if (pthread_sigmask(SIG_BLOCK, &trap, NULL) != 0) {
    return 1;
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0 ||
      pthread_join(thread, NULL) != 0) {
    return 2;
  }
  printf("%ld\n", watched);
  return 0;
}
