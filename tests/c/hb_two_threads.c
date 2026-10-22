/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Two threads of one process each write one line to stdout: a pthread worker
 * writes "worker\n" (7 bytes) and the main thread writes "main!\n" (6 bytes)
 * and then joins it. hermit-cli/tests/cli.rs uses it to check that a
 * --happens-before edge between two threads of one process (the worker's
 * dettid differs from its process's detpid) reverses their order. With the
 * argument "marker" the worker also writes "returned\n" as soon as its first
 * write has returned, so a test can see whether the worker was held after
 * that write (https://github.com/rrnewton/hermit/issues/3929).
 */
#include <pthread.h>
#include <string.h>
#include <unistd.h>

static int marker;

static void *worker(void *arg) {
  (void)arg;
  if (write(1, "worker\n", 7) != 7) {
    _exit(2);
  }
  if (marker && write(1, "returned\n", 9) != 9) {
    _exit(2);
  }
  return 0;
}

int main(int argc, char **argv) {
  marker = argc > 1 && strcmp(argv[1], "marker") == 0;
  pthread_t thread;
  if (pthread_create(&thread, 0, worker, 0) != 0) {
    return 3;
  }
  if (write(1, "main!\n", 6) != 6) {
    return 2;
  }
  return pthread_join(thread, 0) == 0 ? 0 : 4;
}
