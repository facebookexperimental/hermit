/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <unistd.h>

/* A pselect6 with nfds=65 takes Detcore's background path under `run
 * --strict`, and its sibling, in mode `mutable`, rewrites the call's mask
 * buffer before the call reaches the kernel. The scheduler waited for the mask
 * it had read, which the kernel never installed, and withheld the sibling's
 * timer, so the call returned 0 at its 2 s timeout instead of 1 when the pipe
 * was written (Codex review of https://github.com/rrnewton/hermit/pull/4053,
 * whose fixture this is, kept as written). Mode `stable` is the control. */
static int channels[2];
static _Atomic uint64_t wait_mask;
static _Atomic int start;
static int mutate;

struct mask_argument { const void *mask; size_t bytes; };
static void *writer(void *unused) {
  (void)unused;
  while (!atomic_load_explicit(&start, memory_order_acquire))
    sched_yield();
  if (mutate)
    atomic_store_explicit(&wait_mask, UINT64_C(1) << (SIGUSR2 - 1), memory_order_release);
  /* This guest timer needs scheduler progress after the other thread's
   * BACKGROUND grant; an impossible entry-mask barrier must not freeze it. */
  if (usleep(20000) != 0)
    return (void *)(intptr_t)1;
  return (void *)(intptr_t)(write(channels[1], "x", 1) == 1 ? 0 : 2);
}
int main(int argc, char **argv) {
  if (argc != 2 || (strcmp(argv[1], "stable") && strcmp(argv[1], "mutable")))
    return 2;
  mutate = !strcmp(argv[1], "mutable");
  if (pipe(channels) != 0 || channels[0] >= 64 || channels[1] >= 64)
    return 3;
  sigset_t own;
  if (sigemptyset(&own) || sigaddset(&own, SIGUSR1) || pthread_sigmask(SIG_BLOCK, &own, NULL))
    return 4;
  pthread_t worker;
  if (pthread_create(&worker, NULL, writer, NULL))
    return 5;
  fd_set readable;
  FD_ZERO(&readable);
  FD_SET(channels[0], &readable);
  struct mask_argument argument = {&wait_mask, sizeof(uint64_t)};
  struct timespec timeout = {2, 0};
  atomic_store_explicit(&start, 1, memory_order_release);
  errno = 0;
  long result = syscall(SYS_pselect6, 65, &readable, NULL, NULL, &timeout, &argument);
  int saved_errno = result < 0 ? errno : 0;
  int ready = result == 1 && FD_ISSET(channels[0], &readable);
  void *writer_result = (void *)(intptr_t)3;
  int joined = pthread_join(worker, &writer_result) == 0;
  int writer_ok = joined && writer_result == NULL;
  int closed = close(channels[0]) == 0;
  closed = close(channels[1]) == 0 && closed;
  int ok = ready && saved_errno == 0 && writer_ok && closed;
  printf("mode=%s result=%ld errno=%d ready=%d writer_ok=%d oracle=%d\n",
         argv[1], result, saved_errno, ready, writer_ok, ok);
  return ok ? 0 : 42;
}
