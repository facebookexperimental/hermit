/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Cross-process pipe IPC parity probe.
 *
 * The parent creates a pipe and forks. The child writes a fixed byte stream
 * into the pipe (every value 0..255 exactly once), closes the write end, and
 * exits. The parent drains the pipe to EOF, accumulating the byte count and a
 * running checksum, then reaps the child.
 *
 * This exercises the cross-process producer/consumer contract: pipe creation,
 * fd inheritance and per-end close semantics across fork, a blocking read that
 * unblocks when the sole writer closes (EOF), and reaping. It is distinct from
 * the multiprocess_fork_exec row (which exercises fork+execve+exit-status) and
 * from the existing single-process pipe_basics guest (which pipes between
 * threads and prints interleaved, scheduler-dependent lines).
 *
 * It is deliberately free of gated concerns:
 *   - The only observable is an aggregate over the byte stream; no per-message
 *     line is printed, so the result never depends on the parent/child
 *     scheduling interleave.
 *   - No pid, timestamp, cpu-time, or address is observed.
 *
 * For the fixed 0..255 stream the byte count is 256 and the checksum is
 * 255*256/2 = 32640 on any conforming backend.
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define STREAM_BYTES 256

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/* Write the whole buffer, tolerating short writes. */
static int write_all(int fd, const uint8_t *buffer, size_t length) {
  size_t written = 0;
  while (written < length) {
    ssize_t n = write(fd, buffer + written, length - written);
    if (n < 0) {
      if (errno == EINTR)
        continue;
      return -1;
    }
    written += (size_t)n;
  }
  return 0;
}

int main(void) {
  int fds[2];
  if (pipe(fds) != 0)
    fail("pipe");

  pid_t child = fork();
  if (child < 0)
    fail("fork");

  if (child == 0) {
    /* Producer: only the write end is used. */
    if (close(fds[0]) != 0)
      _exit(101);
    uint8_t stream[STREAM_BYTES];
    for (int i = 0; i < STREAM_BYTES; ++i)
      stream[i] = (uint8_t)i;
    if (write_all(fds[1], stream, sizeof(stream)) != 0)
      _exit(102);
    if (close(fds[1]) != 0)
      _exit(103);
    _exit(0);
  }

  /* Consumer: only the read end is used. Closing the write end here is what
   * lets the read below observe EOF once the child also closes it. */
  if (close(fds[1]) != 0)
    fail("close write end");

  long bytes = 0;
  long checksum = 0;
  uint8_t buffer[64];
  for (;;) {
    ssize_t n = read(fds[0], buffer, sizeof(buffer));
    if (n < 0) {
      if (errno == EINTR)
        continue;
      fail("read");
    }
    if (n == 0)
      break; /* EOF: all write ends closed. */
    for (ssize_t i = 0; i < n; ++i) {
      ++bytes;
      checksum += buffer[i];
    }
  }
  if (close(fds[0]) != 0)
    fail("close read end");

  int status = 0;
  if (waitpid(child, &status, 0) != child)
    fail("waitpid");
  int reaped = (WIFEXITED(status) && WEXITSTATUS(status) == 0) ? 1 : 0;

  printf("pipe_ipc bytes=%ld checksum=%ld reaped=%d\n", bytes, checksum, reaped);
  return 0;
}
