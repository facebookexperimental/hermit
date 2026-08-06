/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * copy_file_range(2) deterministic-refusal parity probe.
 *
 * copy_file_range performs an in-kernel byte copy between two file descriptors.
 * Detcore does not implement it on any backend, so every backend must refuse it
 * identically with a deterministic errno rather than performing a host-dependent
 * copy. This mirrors the io_uring and listmount refusal rows: it pins the
 * deterministic error so the syscall cannot silently start copying bytes on one
 * backend while refusing on another.
 *
 * A single process creates a source file holding the six bytes "abcdef" and an
 * empty destination file, then calls copy_file_range to copy them. The contract
 * requires:
 *
 *   - copy_file_range fails with a deterministic errno (ENOSYS), and
 *   - it copies no bytes, so the destination stays empty (size 0).
 *
 * The destination emptiness is verified through fstat, whose file-size result is
 * a pure function of what was written. The printed checksum is derived by
 * reading the source back, independent of copy_file_range, and equals
 * 'a'+'b'+'c'+'d'+'e'+'f' = 597. Only invariants are printed:
 *
 *   copy_file_range_refusal src=6 dst=0 checksum=597 ok=3
 *
 * It is deliberately free of gated concerns: single process, no fork/thread, and
 * no pid, timestamp, cpu-time, inode, device, uid, gid, or address is observed.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define PAYLOAD "abcdef"
#define PAYLOAD_LEN 6

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

static long fd_size(int fd) {
  struct stat st;
  if (fstat(fd, &st) != 0)
    fail("fstat");
  return (long)st.st_size;
}

int main(void) {
  char src_template[] = "/tmp/copy_file_range_src_XXXXXX";
  char dst_template[] = "/tmp/copy_file_range_dst_XXXXXX";

  int src_fd = mkstemp(src_template);
  if (src_fd < 0)
    fail("mkstemp src");
  int dst_fd = mkstemp(dst_template);
  if (dst_fd < 0)
    fail("mkstemp dst");

  if (write(src_fd, PAYLOAD, PAYLOAD_LEN) != PAYLOAD_LEN)
    fail("write payload");

  int ok = 0;

  /* copy_file_range must be refused deterministically with ENOSYS. */
  off_t in_off = 0;
  off_t out_off = 0;
  errno = 0;
  ssize_t copied =
      copy_file_range(src_fd, &in_off, dst_fd, &out_off, PAYLOAD_LEN, 0);
  if (copied < 0 && errno == ENOSYS)
    ok++;

  /* The refusal copied no bytes: the destination is still empty. */
  if (fd_size(dst_fd) == 0)
    ok++;

  /* The source is unchanged and still holds the full payload. */
  if (fd_size(src_fd) == PAYLOAD_LEN)
    ok++;

  /* Derive a content checksum by reading the source, independent of the copy. */
  char buf[PAYLOAD_LEN];
  if (pread(src_fd, buf, PAYLOAD_LEN, 0) != PAYLOAD_LEN)
    fail("pread payload");
  long checksum = 0;
  for (size_t i = 0; i < PAYLOAD_LEN; i++)
    checksum += (unsigned char)buf[i];

  long src_size = fd_size(src_fd);
  long dst_size = fd_size(dst_fd);

  if (close(src_fd) != 0)
    fail("close src");
  if (close(dst_fd) != 0)
    fail("close dst");
  if (unlink(src_template) != 0)
    fail("unlink src");
  if (unlink(dst_template) != 0)
    fail("unlink dst");

  printf("copy_file_range_refusal src=%ld dst=%ld checksum=%ld ok=%d\n",
         src_size, dst_size, checksum, ok);
  return 0;
}
