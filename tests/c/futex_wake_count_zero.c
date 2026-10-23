/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A FUTEX_WAKE count of 0, or one that is negative as the kernel's
 * `int nr_wake` (0xffffffff), wakes one waiter on Linux: futex_wake in
 * kernel/futex/waitwake.c counts a waiter before comparing,
 * `if (++ret >= nr_wake) break;`
 * (https://github.com/rrnewton/hermit/issues/3957).
 *
 * For each count, a thread waits on a private futex and main wakes it with
 * that count until one wake reports a woken waiter (the thread may not be
 * waiting yet on the first tries), then joins it. A wake that never reports a
 * waiter fails the test instead of hanging.
 */

#define _GNU_SOURCE
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

static uint32_t word;

static long futex(uint32_t* uaddr, int op, uint32_t val) {
  return syscall(SYS_futex, uaddr, op, val, NULL, NULL, 0);
}

static void* waiter(void* arg) {
  (void)arg;
  long ret = futex(&word, FUTEX_WAIT_PRIVATE, 0);
  return (void*)ret;
}

static int wake_one_with(uint32_t count) {
  pthread_t thread;
  if (pthread_create(&thread, NULL, waiter, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  long woken = 0;
  for (int attempt = 0; attempt < 10000 && woken == 0; attempt++) {
    woken = futex(&word, FUTEX_WAKE_PRIVATE, count);
    if (woken == 0) {
      sched_yield();
    }
  }
  if (woken != 1) {
    fprintf(stderr, "FUTEX_WAKE count %#x returned %ld, expected 1\n", count, woken);
    return 1;
  }
  void* result;
  pthread_join(thread, &result);
  if ((long)result != 0) {
    fprintf(stderr, "waiter's FUTEX_WAIT returned %ld\n", (long)result);
    return 1;
  }
  printf("count %#x woke 1\n", count);
  return 0;
}

int main(void) {
  if (wake_one_with(0) != 0 || wake_one_with(UINT32_MAX) != 0) {
    return 1;
  }
  long none = futex(&word, FUTEX_WAKE_PRIVATE, 0);
  printf("count 0 with no waiter woke %ld\n", none);
  return none == 0 ? 0 : 1;
}
