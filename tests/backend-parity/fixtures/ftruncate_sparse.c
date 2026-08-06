/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * ftruncate(2) sparse-hole and shrink parity probe.
 *
 * A single process opens one temporary file and drives it through a fixed
 * ftruncate sequence, checking the invariants Detcore's file model must
 * preserve identically on every backend:
 *
 *   - Growing a file with ftruncate extends it with a zero-filled hole: the
 *     original bytes are retained and the newly exposed range reads as zero.
 *   - Shrinking a file with ftruncate discards the bytes beyond the new size,
 *     which fstat then reports.
 *   - Growing again re-exposes a zero-filled hole after the retained prefix.
 *
 * The sequence is:
 *   write "ABCD"        -> size 4, contents "ABCD"
 *   ftruncate to 16     -> size 16, contents "ABCD" + 12 zero bytes
 *   ftruncate to 2      -> size 2,  contents "AB"
 *   ftruncate to 6      -> size 6,  contents "AB" + 4 zero bytes
 *
 * The final six bytes checksum to 'A'+'B' = 131 (the four hole bytes are zero)
 * and the final size is 6. Only invariants are printed:
 *
 *   ftruncate_sparse size=6 checksum=131 ok=6
 *
 * It is deliberately free of gated concerns: single process, no fork/thread, and
 * no pid, timestamp, cpu-time, or address is observed.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

static off_t file_size(int fd) {
  struct stat st;
  if (fstat(fd, &st) != 0)
    fail("fstat");
  return st.st_size;
}

/* Read exactly n bytes at offset 0 into buf. */
static void read_all_at0(int fd, char *buf, size_t n) {
  size_t got = 0;
  while (got < n) {
    ssize_t r = pread(fd, buf + got, n - got, (off_t)got);
    if (r < 0) {
      if (errno == EINTR)
        continue;
      fail("pread");
    }
    if (r == 0)
      fail("pread short (unexpected EOF)");
    got += (size_t)r;
  }
}

/* Return 1 if buf[from..to) are all zero. */
static int all_zero(const char *buf, size_t from, size_t to) {
  for (size_t i = from; i < to; i++)
    if (buf[i] != 0)
      return 0;
  return 1;
}

int main(void) {
  char template[] = "/tmp/ftruncate_sparse_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (unlink(template) != 0)
    fail("unlink");

  int ok = 0;

  if (write(fd, "ABCD", 4) != 4)
    fail("write ABCD");

  /* Grow: original bytes retained, new range is a zero-filled hole. */
  if (ftruncate(fd, 16) != 0)
    fail("ftruncate grow 16");
  if (file_size(fd) == 16)
    ok++;
  char grown[16];
  read_all_at0(fd, grown, sizeof(grown));
  if (memcmp(grown, "ABCD", 4) == 0)
    ok++;
  if (all_zero(grown, 4, 16))
    ok++;

  /* Shrink: bytes beyond the new size are discarded. */
  if (ftruncate(fd, 2) != 0)
    fail("ftruncate shrink 2");
  if (file_size(fd) == 2)
    ok++;
  char shrunk[2];
  read_all_at0(fd, shrunk, sizeof(shrunk));
  if (memcmp(shrunk, "AB", 2) == 0)
    ok++;

  /* Grow again: retained prefix plus a fresh zero-filled hole. */
  if (ftruncate(fd, 6) != 0)
    fail("ftruncate grow 6");
  char regrown[6];
  read_all_at0(fd, regrown, sizeof(regrown));
  if (memcmp(regrown, "AB", 2) == 0 && all_zero(regrown, 2, 6))
    ok++;

  off_t final_size = file_size(fd);
  long checksum = 0;
  for (size_t i = 0; i < sizeof(regrown); i++)
    checksum += (unsigned char)regrown[i];

  if (close(fd) != 0)
    fail("close");

  printf("ftruncate_sparse size=%ld checksum=%ld ok=%d\n", (long)final_size,
         checksum, ok);
  return 0;
}
