/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/random.h>
#include <sys/wait.h>
#include <unistd.h>

enum { CHILDREN = 2, BYTES = 16 };

static void fail(const char* operation) {
  perror(operation);
  exit(1);
}

static void write_exact(int fd, const uint8_t* bytes, size_t length) {
  size_t offset = 0;
  while (offset < length) {
    ssize_t written = write(fd, bytes + offset, length - offset);
    if (written < 0 && errno == EINTR) {
      continue;
    }
    if (written <= 0) {
      _exit(2);
    }
    offset += (size_t)written;
  }
}

static void read_exact(int fd, uint8_t* bytes, size_t length) {
  size_t offset = 0;
  while (offset < length) {
    ssize_t count = read(fd, bytes + offset, length - offset);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      fail("read");
    }
    offset += (size_t)count;
  }
}

int main(void) {
  uint8_t samples[CHILDREN][BYTES];

  for (int child_index = 0; child_index < CHILDREN; child_index++) {
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
      fail("pipe");
    }

    pid_t child = fork();
    if (child < 0) {
      fail("fork");
    }
    if (child == 0) {
      close(pipe_fds[0]);
      uint8_t bytes[BYTES];
      if (getrandom(bytes, sizeof(bytes), GRND_NONBLOCK) !=
          (ssize_t)sizeof(bytes)) {
        _exit(3);
      }
      write_exact(pipe_fds[1], bytes, sizeof(bytes));
      close(pipe_fds[1]);
      _exit(0);
    }

    close(pipe_fds[1]);
    read_exact(pipe_fds[0], samples[child_index], BYTES);
    close(pipe_fds[0]);

    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
      return 4;
    }
  }

  int distinct = 0;
  for (int byte = 0; byte < BYTES; byte++) {
    distinct |= samples[0][byte] != samples[1][byte];
  }
  if (!distinct) {
    fputs("fork children reused one RNG stream\n", stderr);
    return 5;
  }

  for (int child_index = 0; child_index < CHILDREN; child_index++) {
    printf("child-%d=", child_index);
    for (int byte = 0; byte < BYTES; byte++) {
      printf("%02x", samples[child_index][byte]);
    }
    putchar('\n');
  }
  return 0;
}
