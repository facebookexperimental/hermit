/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Vectored regular-file I/O parity probe.
 *
 * A single process drives scatter/gather I/O against one temporary regular file
 * and checks the invariants Detcore's file model must preserve identically on
 * every backend:
 *
 *   - writev(2) gathers the iovecs in order into a contiguous file region and
 *     advances the file offset by the total.
 *   - readv(2) scatters a contiguous region back across the iovecs in order.
 *   - pwritev(2)/preadv(2) apply at an explicit offset without disturbing the
 *     descriptor's current file offset.
 *
 * The sequence is:
 *   writev "abc"+"defgh"+"ij"   -> file "abcdefghij" (size 10, offset 10)
 *   lseek 0; readv [4][6]       -> "abcd" + "efghij"
 *   pwritev "WX"+"YZ" @off 4    -> file "abcdWXYZij" (offset unchanged at 10)
 *   preadv [5][5] @off 0        -> "abcdW" + "XYZij"
 *
 * The final ten bytes "abcdWXYZij" checksum to 959 and the final size is 10.
 * Only invariants are printed:
 *
 *   vectored_file_io size=10 checksum=959 ok=6
 *
 * It is deliberately free of gated concerns: single process, no fork/thread, and
 * no pid, timestamp, cpu-time, or address is observed.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

int main(void) {
  char template[] = "/tmp/vectored_file_io_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (unlink(template) != 0)
    fail("unlink");

  int ok = 0;

  /* writev gathers the three iovecs contiguously into the file. */
  struct iovec wv[3] = {
      {(void *)"abc", 3},
      {(void *)"defgh", 5},
      {(void *)"ij", 2},
  };
  if (writev(fd, wv, 3) == 10)
    ok++;
  if (lseek(fd, 0, SEEK_END) == 10) /* offset advanced by the total */
    ok++;

  /* readv scatters the contiguous region back across two buffers. */
  if (lseek(fd, 0, SEEK_SET) != 0)
    fail("lseek 0");
  char r1[4];
  char r2[6];
  struct iovec rv[2] = {{r1, sizeof(r1)}, {r2, sizeof(r2)}};
  if (readv(fd, rv, 2) == 10 && memcmp(r1, "abcd", 4) == 0 &&
      memcmp(r2, "efghij", 6) == 0)
    ok++;
  off_t after_readv = lseek(fd, 0, SEEK_CUR); /* should be 10 */

  /* pwritev overwrites [4..8) at an explicit offset, not the file offset. */
  struct iovec pw[2] = {{(void *)"WX", 2}, {(void *)"YZ", 2}};
  if (pwritev(fd, pw, 2, 4) == 4)
    ok++;
  if (lseek(fd, 0, SEEK_CUR) == after_readv) /* pwritev left the offset alone */
    ok++;

  /* preadv reads the whole file back at an explicit offset. */
  char p1[5];
  char p2[5];
  struct iovec pr[2] = {{p1, sizeof(p1)}, {p2, sizeof(p2)}};
  if (preadv(fd, pr, 2, 0) == 10 && memcmp(p1, "abcdW", 5) == 0 &&
      memcmp(p2, "XYZij", 5) == 0)
    ok++;

  struct stat st;
  if (fstat(fd, &st) != 0)
    fail("fstat");
  char whole[10];
  memcpy(whole, p1, 5);
  memcpy(whole + 5, p2, 5);
  long checksum = 0;
  for (size_t i = 0; i < sizeof(whole); i++)
    checksum += (unsigned char)whole[i];

  if (close(fd) != 0)
    fail("close");

  printf("vectored_file_io size=%ld checksum=%ld ok=%d\n", (long)st.st_size,
         checksum, ok);
  return 0;
}
