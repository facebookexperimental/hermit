/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Exercises the guest-visible contract for a /proc sanitize refusal
 * under --allow-unsupported-syscalls: an epoll fdinfo file with a
 * registered target, a kind Detcore's fdinfo policy does not serve,
 * must fail EVERY read with EIO -- the first, a repeat read, a
 * pread, and a read through a dup'd descriptor -- and never read as
 * an empty file. (In strict mode the run stops with the named
 * policy refusal at the first read; the harness asserts that from
 * outside.)
 *
 * The probe prints only fixed strings.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <unistd.h>

static int expect_eio(const char* label, int fd, int positioned) {
  char buf[256];
  ssize_t n = positioned ? pread(fd, buf, sizeof(buf), 0)
                         : read(fd, buf, sizeof(buf));
  if (n >= 0) {
    fprintf(stderr, "%s: read returned %zd, expected EIO\n", label, n);
    return 1;
  }
  if (errno != EIO) {
    fprintf(stderr, "%s: errno %d, expected EIO\n", label, errno);
    return 1;
  }
  printf("procfs-refusal-%s-eio\n", label);
  fflush(stdout);
  return 0;
}

static int expect_splice_einval(const char* label, const char* path) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    perror("open for splice");
    return 3;
  }
  int pipefd[2];
  if (pipe(pipefd) < 0) {
    perror("pipe for splice");
    return 3;
  }
  errno = 0;
  ssize_t spliced = splice(fd, NULL, pipefd[1], NULL, 16, 0);
  if (spliced >= 0) {
    fprintf(stderr, "%s: returned %zd, expected EINVAL\n", label, spliced);
    return 1;
  }
  if (errno != EINVAL) {
    fprintf(stderr, "%s: errno %d, expected EINVAL\n", label, errno);
    return 1;
  }
  int flags = fcntl(pipefd[0], F_GETFL);
  char drain[16];
  ssize_t drained = -2;
  if (flags >= 0 && fcntl(pipefd[0], F_SETFL, flags | O_NONBLOCK) == 0) {
    drained = read(pipefd[0], drain, sizeof(drain));
  }
  if (drained != -1 || errno != EAGAIN) {
    fprintf(stderr, "%s: pipe not empty (read %zd)\n", label, drained);
    return 1;
  }
  printf("procfs-refusal-%s-einval\n", label);
  fflush(stdout);
  return 0;
}

int main(void) {
  int pipefd[2];
  if (pipe(pipefd) < 0) {
    perror("pipe");
    return 3;
  }
  int ep = epoll_create1(0);
  if (ep < 0) {
    perror("epoll_create1");
    return 3;
  }
  struct epoll_event event = {.events = EPOLLIN, .data.fd = pipefd[0]};
  if (epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[0], &event) < 0) {
    perror("epoll_ctl");
    return 3;
  }

  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", ep);
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    perror("open fdinfo");
    return 3;
  }
  printf("procfs-refusal-probe-opened\n");
  fflush(stdout);

  int rc = 0;
  rc |= expect_eio("read1", fd, 0);
  rc |= expect_eio("read2", fd, 0);
  rc |= expect_eio("pread", fd, 1);
  int alias = dup(fd);
  if (alias < 0) {
    perror("dup");
    return 3;
  }
  rc |= expect_eio("dup", alias, 0);
  // splice from the refused fd must fail the same way, not copy
  // live kernel bytes into the pipe.
  int splice_pipe[2];
  if (pipe(splice_pipe) < 0) {
    perror("pipe for splice");
    return 3;
  }
  errno = 0;
  ssize_t spliced = splice(fd, NULL, splice_pipe[1], NULL, 16, 0);
  if (spliced >= 0) {
    fprintf(stderr, "splice: returned %zd, expected EIO\n", spliced);
    rc |= 1;
  } else if (errno != EIO) {
    fprintf(stderr, "splice: errno %d, expected EIO\n", errno);
    rc |= 1;
  } else {
    printf("procfs-refusal-splice-eio\n");
    fflush(stdout);
  }
  // A splice from a valid (unrefused) procfs snapshot must also
  // not pass host bytes: it refuses EINVAL so callers fall back to
  // the mediated read path, and the pipe stays empty. Status is
  // the seat's case; uptime is the discriminating one (the kernel
  // splices it natively, so only Detcore's refusal can produce
  // EINVAL with an empty pipe here).
  rc |= expect_splice_einval("status-splice", "/proc/self/status");
  rc |= expect_splice_einval("uptime-splice", "/proc/uptime");
  return rc;
}
