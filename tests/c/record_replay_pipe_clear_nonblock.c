/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A guest that turns O_NONBLOCK on and then off again on a pipe, once with
 * ioctl(FIONBIO) and once with fcntl(F_SETFL), and then blocks reading it
 * until a sibling thread writes. Hermit keeps container-internal pipes
 * physically nonblocking so that a blocking read can wait deterministically;
 * clearing the guest's flag must not clear that physical state.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

static int fail(const char* operation) {
  fprintf(stderr, "%s failed: %s\n", operation, strerror(errno));
  return 1;
}

static void* write_later(void* arg) {
  int fd = *(int*)arg;
  struct timespec delay = {.tv_sec = 0, .tv_nsec = 10 * 1000 * 1000};
  nanosleep(&delay, NULL);
  if (write(fd, "go", 2) != 2) {
    perror("write");
  }
  return NULL;
}

static int set_with_fionbio(int fd, int enabled) {
  return ioctl(fd, FIONBIO, &enabled);
}

static int set_with_setfl(int fd, int enabled) {
  int flags = fcntl(fd, F_GETFL);
  if (flags < 0) {
    return -1;
  }
  flags = enabled ? (flags | O_NONBLOCK) : (flags & ~O_NONBLOCK);
  return fcntl(fd, F_SETFL, flags);
}

static int exercise(const char* name, int (*set_nonblocking)(int, int)) {
  int fds[2];
  if (pipe(fds) != 0) {
    return fail("pipe");
  }
  if (set_nonblocking(fds[0], 1) != 0) {
    return fail("set nonblocking");
  }
  char byte;
  errno = 0;
  ssize_t empty = read(fds[0], &byte, 1);
  int empty_errno = errno;
  if (set_nonblocking(fds[0], 0) != 0) {
    return fail("clear nonblocking");
  }
  int flags = fcntl(fds[0], F_GETFL);
  if (flags < 0) {
    return fail("F_GETFL");
  }

  pthread_t writer;
  if (pthread_create(&writer, NULL, write_later, &fds[1]) != 0) {
    return fail("pthread_create");
  }
  char buffer[2];
  ssize_t got = read(fds[0], buffer, sizeof(buffer));
  int got_errno = errno;
  pthread_join(writer, NULL);

  printf(
      "%s: empty read %zd (%s), O_NONBLOCK after clear %d, blocking read %zd \"%.*s\"%s%s\n",
      name,
      empty,
      empty_errno == EAGAIN ? "EAGAIN" : strerror(empty_errno),
      (flags & O_NONBLOCK) != 0,
      got,
      got > 0 ? (int)got : 0,
      buffer,
      got < 0 ? " errno " : "",
      got < 0 ? strerror(got_errno) : "");
  close(fds[0]);
  close(fds[1]);
  return empty == -1 && empty_errno == EAGAIN && (flags & O_NONBLOCK) == 0 &&
          got == 2 && memcmp(buffer, "go", 2) == 0
      ? 0
      : 1;
}

int main(void) {
  int status = exercise("FIONBIO", set_with_fionbio);
  status |= exercise("F_SETFL", set_with_setfl);
  return status;
}
