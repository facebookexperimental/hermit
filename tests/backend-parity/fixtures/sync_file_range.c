/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/*
 * sync_file_range(2) is a Linux-specific writeback-durability barrier: it flushes
 * a byte range of a file's page cache to the backing store without the whole-file
 * scope of fsync/fdatasync (the fsync_durability contract) or the mount-wide scope
 * of syncfs. Like those barriers it has no observable effect on file DATA -- it is
 * a hint to the kernel about when dirty pages reach disk -- so this contract
 * asserts only return values, which are deterministic across repeated runs and
 * backends: a valid range flush returns 0 and a bad descriptor returns -1/EBADF.
 *
 * It touches no time, randomness, scheduling, or signal channel. Every barrier is
 * issued on a small file that was just written, so writeback completes promptly
 * and the WAIT flags do not turn into an unbounded block.
 *
 * ptrace and DBT drive the full barrier set; if KVM's ElfExecutor does not
 * implement sync_file_range it refuses deterministically with ENOSYS, recorded as
 * a KVM gap in matrix.tsv (mirrors the syncfs gap in fsync_durability).
 */

#ifndef SYNC_FILE_RANGE_WAIT_BEFORE
#define SYNC_FILE_RANGE_WAIT_BEFORE 1
#endif
#ifndef SYNC_FILE_RANGE_WRITE
#define SYNC_FILE_RANGE_WRITE 2
#endif
#ifndef SYNC_FILE_RANGE_WAIT_AFTER
#define SYNC_FILE_RANGE_WAIT_AFTER 4
#endif

int main(void) {
  int ok = 0;

  char tmpl[] = "/tmp/parity_syncrange_XXXXXX";
  int fd = mkstemp(tmpl);
  if (fd < 0) {
    fprintf(stderr, "mkstemp errno %d\n", errno);
    return 1;
  }

  /* Dirty two pages so the barriers have real writeback to schedule. */
  char buf[8192];
  memset(buf, 'S', sizeof(buf));
  if (write(fd, buf, sizeof(buf)) != (ssize_t)sizeof(buf)) {
    fprintf(stderr, "write errno %d\n", errno);
    unlink(tmpl);
    return 1;
  }

  /* Async writeback of the whole file (offset 0, nbytes 0 == to end of file). */
  if (sync_file_range(fd, 0, 0, SYNC_FILE_RANGE_WRITE) == 0) {
    ok++;
  } else {
    fprintf(stderr, "sync_file_range WRITE errno %d\n", errno);
    unlink(tmpl);
    return 1;
  }

  /* Full barrier on the first page: wait-before, write, wait-after. */
  if (sync_file_range(
          fd,
          0,
          4096,
          SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE |
              SYNC_FILE_RANGE_WAIT_AFTER) == 0) {
    ok++;
  } else {
    fprintf(stderr, "sync_file_range full-barrier errno %d\n", errno);
    unlink(tmpl);
    return 1;
  }

  /* Async writeback of the second page only (offset 4096). */
  if (sync_file_range(fd, 4096, 4096, SYNC_FILE_RANGE_WRITE) == 0) {
    ok++;
  } else {
    fprintf(stderr, "sync_file_range second-page errno %d\n", errno);
    unlink(tmpl);
    return 1;
  }

  /* A closed/invalid descriptor is a deterministic EBADF. */
  if (sync_file_range(-1, 0, 0, SYNC_FILE_RANGE_WRITE) == -1 && errno == EBADF) {
    ok++;
  } else {
    fprintf(stderr, "sync_file_range(-1) errno %d\n", errno);
    unlink(tmpl);
    return 1;
  }

  close(fd);
  unlink(tmpl);
  printf("syncrange ok=%d\n", ok);
  return 0;
}
