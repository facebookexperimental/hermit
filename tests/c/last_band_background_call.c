/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Main makes a call that Detcore runs in the background and that fails at once
// (connect on a bad descriptor, whose errno Linux supplies) while a worker
// takes 400 steps, each one scheduler turn (sched_yield). No signal ends the call, so its finish is
// a completion only the host observes. Run mode admits such a completion only
// when no ordinary work is runnable. Under --chaos-target-races the worker can
// sit in the last priority band; while that band alone counted as a poller,
// the call was admitted between the worker's steps whenever the host finished
// it, so the progress printed varied from run to run
// (https://github.com/rrnewton/hermit/issues/4234). Prints the worker's
// progress when main's call returned.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static atomic_long progress;

static void* work(void* unused) {
  (void)unused;
  for (long i = 0; i < 400; i++) {
    atomic_store(&progress, i);
    sched_yield();
  }
  return NULL;
}

int main(void) {
  pthread_t worker;
  if (pthread_create(&worker, NULL, work, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  struct sockaddr_in address;
  memset(&address, 0, sizeof(address));
  address.sin_family = AF_INET;
  address.sin_port = htons(9);
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  int ret = connect(-1, (struct sockaddr*)&address, sizeof(address));
  int error = errno;
  long seen = atomic_load(&progress);
  pthread_join(worker, NULL);
  printf("connect %d errno %d, worker progress %ld\n", ret, ret == -1 ? error : 0, seen);
  return 0;
}
