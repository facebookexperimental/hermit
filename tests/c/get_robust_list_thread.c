/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <linux/futex.h>
#include <pthread.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

static int tid_pipe[2];
static int release_pipe[2];

static void *worker(void *unused) {
  (void)unused;
  pid_t tid = (pid_t)syscall(SYS_gettid);
  char release = 0;

  if (write(tid_pipe[1], &tid, sizeof(tid)) != sizeof(tid) ||
      read(release_pipe[0], &release, sizeof(release)) != sizeof(release)) {
    return (void *)1;
  }
  return NULL;
}

int main(void) {
  pthread_t thread;
  pid_t tid = 0;
  struct robust_list_head *head = NULL;
  size_t length = 0;

  if (pipe(tid_pipe) != 0 || pipe(release_pipe) != 0 ||
      pthread_create(&thread, NULL, worker, NULL) != 0) {
    perror("pthread setup");
    return 1;
  }
  if (read(tid_pipe[0], &tid, sizeof(tid)) != sizeof(tid) ||
      syscall(SYS_get_robust_list, tid, &head, &length) != 0) {
    perror("get_robust_list thread");
    return 1;
  }
  if (head == NULL || length != sizeof(*head)) {
    fprintf(stderr, "unexpected thread robust-list shape\n");
    return 1;
  }

  char release = 1;
  if (write(release_pipe[1], &release, sizeof(release)) != sizeof(release)) {
    perror("worker release");
    return 1;
  }

  void *result = NULL;
  if (pthread_join(thread, &result) != 0 || result != NULL) {
    fprintf(stderr, "worker failed\n");
    return 1;
  }

  puts("thread robust list queried");
  return 0;
}
