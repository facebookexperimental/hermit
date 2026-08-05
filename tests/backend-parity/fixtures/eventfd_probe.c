/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * eventfd counter-semantics parity probe.
 *
 * A single process exercises both eventfd(2) modes:
 *   - Default counter mode: eight writes (values 1..8) accumulate into the
 *     64-bit counter; a single read returns the sum (36) and resets it.
 *   - Semaphore mode (EFD_SEMAPHORE): one write of 5 seeds the counter; five
 *     reads each return 1 and decrement, summing to 5.
 *
 * This covers the eventfd2 syscall and its add-on-write / drain-on-read
 * arithmetic, which the other rows do not touch. Every read is issued only
 * after enough has been written to satisfy it, so no read ever blocks and the
 * program has no scheduling or timing dependence.
 *
 * It is deliberately free of gated concerns:
 *   - Single process, no fork/thread: no scheduling interleave is observed.
 *   - The only observable is the fixed counter arithmetic; no pid, timestamp,
 *     cpu-time, or address is observed.
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

static int write_u64(int fd, uint64_t value) {
  for (;;) {
    ssize_t n = write(fd, &value, sizeof(value));
    if (n == (ssize_t)sizeof(value))
      return 0;
    if (n < 0 && errno == EINTR)
      continue;
    return -1;
  }
}

static int read_u64(int fd, uint64_t *out) {
  for (;;) {
    ssize_t n = read(fd, out, sizeof(*out));
    if (n == (ssize_t)sizeof(*out))
      return 0;
    if (n < 0 && errno == EINTR)
      continue;
    return -1;
  }
}

int main(void) {
  /* Default counter mode: writes accumulate, one read drains the total. */
  int counter_fd = eventfd(0, EFD_CLOEXEC);
  if (counter_fd < 0)
    fail("eventfd counter");
  for (uint64_t i = 1; i <= 8; ++i) {
    if (write_u64(counter_fd, i) != 0)
      fail("write counter");
  }
  uint64_t counter = 0;
  if (read_u64(counter_fd, &counter) != 0)
    fail("read counter");
  if (close(counter_fd) != 0)
    fail("close counter");

  /* Semaphore mode: one seed write, one read per unit. */
  int sem_fd = eventfd(0, EFD_CLOEXEC | EFD_SEMAPHORE);
  if (sem_fd < 0)
    fail("eventfd semaphore");
  if (write_u64(sem_fd, 5) != 0)
    fail("write semaphore");
  uint64_t sem_sum = 0;
  for (int i = 0; i < 5; ++i) {
    uint64_t unit = 0;
    if (read_u64(sem_fd, &unit) != 0)
      fail("read semaphore");
    sem_sum += unit;
  }
  if (close(sem_fd) != 0)
    fail("close semaphore");

  printf("eventfd counter=%llu sem=%llu\n", (unsigned long long)counter,
         (unsigned long long)sem_sum);
  return 0;
}
