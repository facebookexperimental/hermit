/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * I/O readiness (poll/select/ppoll) parity probe.
 *
 * A single process creates one pipe and drives it through a fixed sequence of
 * states, querying readiness with a ZERO timeout at each step so the result is a
 * pure function of the pipe's buffered state and never a function of elapsed
 * time. It checks the invariants Detcore's readiness model must preserve
 * identically on every backend:
 *
 *   - A pipe with buffered data reports the read end readable (POLLIN) and the
 *     write end writable (POLLOUT) under both poll(2) and select(2).
 *   - After the buffered data is drained, with the write end still open, the
 *     read end reports NOT ready with a zero timeout (poll returns 0).
 *   - After the write end is closed, the read end reports an event: poll returns
 *     a nonzero revents mask that includes POLLHUP (and/or POLLIN for the EOF).
 *   - ppoll(2) with a zero timespec agrees with poll(2) on the writable case.
 *
 * A zero timeout means the calls return immediately; no wall-clock, monotonic
 * time, pid, or address is ever observed. The observable is an aggregate of
 * boolean checks:
 *
 *   poll_readiness ok=8
 *
 * It is deliberately free of gated concerns: single process, no fork/thread, and
 * no timestamp, cpu-time, pid, or address is observed.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <time.h>
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/* poll a single fd for `events` with a zero timeout; return the revents mask
 * (poll must return 0 or 1 for one fd). */
static short poll_one(int fd, short events) {
  struct pollfd pfd = {.fd = fd, .events = events, .revents = 0};
  int rc;
  do {
    rc = poll(&pfd, 1, 0);
  } while (rc < 0 && errno == EINTR);
  if (rc < 0)
    fail("poll");
  return (rc == 0) ? 0 : pfd.revents;
}

/* select a single read fd with a zero timeout; return 1 if readable. */
static int select_readable(int fd) {
  fd_set rfds;
  int rc;
  do {
    FD_ZERO(&rfds);
    FD_SET(fd, &rfds);
    struct timeval tv = {.tv_sec = 0, .tv_usec = 0};
    rc = select(fd + 1, &rfds, NULL, NULL, &tv);
  } while (rc < 0 && errno == EINTR);
  if (rc < 0)
    fail("select");
  return (rc > 0 && FD_ISSET(fd, &rfds)) ? 1 : 0;
}

/* select a single write fd with a zero timeout; return 1 if writable. */
static int select_writable(int fd) {
  fd_set wfds;
  int rc;
  do {
    FD_ZERO(&wfds);
    FD_SET(fd, &wfds);
    struct timeval tv = {.tv_sec = 0, .tv_usec = 0};
    rc = select(fd + 1, NULL, &wfds, NULL, &tv);
  } while (rc < 0 && errno == EINTR);
  if (rc < 0)
    fail("select");
  return (rc > 0 && FD_ISSET(fd, &wfds)) ? 1 : 0;
}

/* ppoll a single fd for `events` with a zero timespec; return the revents. */
static short ppoll_one(int fd, short events) {
  struct pollfd pfd = {.fd = fd, .events = events, .revents = 0};
  struct timespec zero = {.tv_sec = 0, .tv_nsec = 0};
  int rc;
  do {
    rc = ppoll(&pfd, 1, &zero, NULL);
  } while (rc < 0 && errno == EINTR);
  if (rc < 0)
    fail("ppoll");
  return (rc == 0) ? 0 : pfd.revents;
}

int main(void) {
  int fds[2];
  if (pipe(fds) != 0)
    fail("pipe");
  int rd = fds[0];
  int wr = fds[1];

  int ok = 0;

  /* Buffer some data so the read end becomes readable. */
  if (write(wr, "hello", 5) != 5)
    fail("write");

  /* poll: read end readable, write end writable. */
  if (poll_one(rd, POLLIN) & POLLIN)
    ok++;
  if (poll_one(wr, POLLOUT) & POLLOUT)
    ok++;

  /* select agrees on both. */
  if (select_readable(rd))
    ok++;
  if (select_writable(wr))
    ok++;

  /* ppoll agrees on the writable case with a zero timespec. */
  if (ppoll_one(wr, POLLOUT) & POLLOUT)
    ok++;

  /* Drain the buffered data; with the write end still open the read end must
   * report NOT ready under a zero timeout. */
  char buf[8];
  ssize_t got = read(rd, buf, sizeof(buf));
  if (got != 5)
    fail("read");
  if (poll_one(rd, POLLIN) == 0)
    ok++;
  if (!select_readable(rd))
    ok++;

  /* Close the write end; the read end now reports EOF via POLLHUP/POLLIN. */
  if (close(wr) != 0)
    fail("close wr");
  short revents = poll_one(rd, POLLIN);
  if (revents & (POLLHUP | POLLIN))
    ok++;

  if (close(rd) != 0)
    fail("close rd");

  printf("poll_readiness ok=%d\n", ok);
  return 0;
}
