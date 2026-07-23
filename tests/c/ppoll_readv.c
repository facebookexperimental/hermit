/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Record/replay guest for GH #22: exercises ppoll and vectored reads (readv).
 *
 * Using a self-pipe (no external I/O), it:
 *   1. writes a known payload to the pipe,
 *   2. waits for readability with ppoll (a non-null pollfd array; checks
 *      revents and the return count),
 *   3. reads the payload with readv split across three iovecs, verifying the
 *      bytes are scattered correctly, and
 *   4. calls ppoll on the now-empty pipe with a short timeout to exercise the
 *      timeout path (return value 0, POLLIN not set).
 *
 * Deterministic under Hermit; the recorder must capture the ppoll revents and
 * the readv output bytes so replay reproduces them without touching live fds.
 */

#define _GNU_SOURCE
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

static const char PAYLOAD[] = "ABCDEFGHIJKL"; /* 12 bytes, no NUL read/written */
#define PAYLOAD_LEN 12

int main(void) {
  int fds[2];
  if (pipe(fds) != 0) {
    perror("pipe");
    return 1;
  }

  if (write(fds[1], PAYLOAD, PAYLOAD_LEN) != (ssize_t)PAYLOAD_LEN) {
    perror("write");
    return 1;
  }

  /* 1. ppoll for readability with a non-null signal mask (block SIGUSR1). */
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  struct pollfd pfd = {.fd = fds[0], .events = POLLIN, .revents = 0};
  struct timespec ready_timeout = {.tv_sec = 5, .tv_nsec = 0};
  int ready = ppoll(&pfd, 1, &ready_timeout, &mask);
  if (ready != 1 || !(pfd.revents & POLLIN)) {
    fprintf(stderr, "ppoll(ready): ret=%d revents=%d\n", ready, pfd.revents);
    return 1;
  }

  /* 2. readv the payload split across three iovecs (5 + 4 + 3 == 12). */
  char a[5] = {0};
  char b[4] = {0};
  char c[3] = {0};
  struct iovec iov[3] = {
      {.iov_base = a, .iov_len = sizeof(a)},
      {.iov_base = b, .iov_len = sizeof(b)},
      {.iov_base = c, .iov_len = sizeof(c)},
  };
  ssize_t got = readv(fds[0], iov, 3);
  if (got != (ssize_t)PAYLOAD_LEN) {
    fprintf(stderr, "readv: got=%zd\n", got);
    return 1;
  }
  char joined[PAYLOAD_LEN];
  memcpy(joined, a, 5);
  memcpy(joined + 5, b, 4);
  memcpy(joined + 9, c, 3);
  if (memcmp(joined, PAYLOAD, PAYLOAD_LEN) != 0) {
    fprintf(stderr, "readv payload mismatch\n");
    return 1;
  }

  /* 3. ppoll timeout path on the now-empty pipe (return 0). */
  struct pollfd empty = {.fd = fds[0], .events = POLLIN, .revents = 0};
  struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
  int timed_out = ppoll(&empty, 1, &zero, NULL);
  if (timed_out != 0) {
    fprintf(stderr, "ppoll(timeout): ret=%d revents=%d\n", timed_out,
            empty.revents);
    return 1;
  }

  printf("ppoll-readv-ok %.*s\n", PAYLOAD_LEN, joined);
  return 0;
}
