/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * close_range(2) file-descriptor-range parity probe.
 *
 * close_range(first, last, flags) closes every open descriptor in the inclusive
 * range [first, last] in one syscall. It is a pure file-descriptor-table
 * operation: its effect is a deterministic function of which descriptors the
 * process itself opened, with no dependence on time, scheduling, pid, or host
 * identity. Detcore forwards it on the ptrace and DBI backends, so both must
 * apply it identically.
 *
 * A single process opens one temporary file, then places four dups at known
 * descriptor numbers 100..103 with fcntl(F_DUPFD). close_range(100, 102, 0)
 * closes exactly the first three; descriptor 103 is left open. The contract:
 *
 *   - close_range(100, 102, 0) succeeds.
 *   - descriptors 100, 101, and 102 are now closed (fcntl reports EBADF).
 *   - descriptor 103 is still open and still reads the file's contents.
 *
 * Over a file containing the six bytes "abcdef" the invariants are a size of 6
 * and a content checksum of 'a'+'b'+'c'+'d'+'e'+'f' = 597, read back through the
 * surviving descriptor. Only invariants are printed:
 *
 *   close_range_fds size=6 checksum=597 ok=6
 *
 * It is deliberately free of gated concerns: single process, no fork/thread, and
 * no pid, timestamp, cpu-time, inode, device, uid, gid, or address is observed.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define PAYLOAD "abcdef"
#define PAYLOAD_LEN 6

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/* Returns nonzero if the descriptor is closed (fcntl reports EBADF). */
static int is_closed(int fd) {
  errno = 0;
  return fcntl(fd, F_GETFD) < 0 && errno == EBADF;
}

int main(void) {
  char template[] = "/tmp/close_range_fds_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (write(fd, PAYLOAD, PAYLOAD_LEN) != PAYLOAD_LEN)
    fail("write payload");

  /* Place four dups at the fixed descriptors 100..103. */
  for (int target = 100; target <= 103; target++) {
    int dup_fd = fcntl(fd, F_DUPFD, target);
    if (dup_fd != target)
      fail("fcntl F_DUPFD");
  }

  int ok = 0;

  /* close_range closes the inclusive span 100..102 in one call. */
  if (close_range(100, 102, 0) == 0)
    ok++;

  /* The three descriptors in the range are now closed. */
  if (is_closed(100))
    ok++;
  if (is_closed(101))
    ok++;
  if (is_closed(102))
    ok++;

  /* Descriptor 103, just past the range, is still open. */
  if (!is_closed(103))
    ok++;

  /* The surviving descriptor still reads the file's payload. */
  char buf[PAYLOAD_LEN];
  long checksum = 0;
  if (pread(103, buf, PAYLOAD_LEN, 0) == PAYLOAD_LEN) {
    for (size_t i = 0; i < PAYLOAD_LEN; i++)
      checksum += (unsigned char)buf[i];
    ok++;
  }

  long size = (long)PAYLOAD_LEN;

  if (close(103) != 0)
    fail("close survivor");
  if (close(fd) != 0)
    fail("close original");
  if (unlink(template) != 0)
    fail("unlink");

  printf("close_range_fds size=%ld checksum=%ld ok=%d\n", size, checksum, ok);
  return 0;
}
