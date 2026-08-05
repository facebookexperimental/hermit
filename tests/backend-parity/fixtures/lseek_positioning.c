/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * lseek(2) file-positioning parity probe.
 *
 * A single process opens one temporary file and exercises the deterministic
 * arithmetic of the three lseek whences plus seek-past-EOF hole creation,
 * checking the invariants Detcore's file model must preserve identically on
 * every backend:
 *
 *   - SEEK_SET sets the absolute offset; a subsequent read returns the bytes at
 *     that offset and advances the offset.
 *   - SEEK_CUR adds a signed delta to the current offset.
 *   - SEEK_END adds a signed delta to the file size, so it can report the size
 *     (delta 0) or address the last byte (delta -1).
 *   - Seeking past end-of-file and writing leaves a zero-filled hole between the
 *     old end and the new data; the file size grows to cover the written range.
 *
 * The sequence over the initial contents "0123456789" (size 10) is:
 *   lseek(0, SEEK_END)  -> 10   (size via seek)
 *   lseek(3, SEEK_SET)  -> 3;   read 2 -> "34"  (offset now 5)
 *   lseek(2, SEEK_CUR)  -> 7;   read 1 -> "7"   (offset now 8)
 *   lseek(-1, SEEK_END) -> 9;   read 1 -> "9"
 *   lseek(14, SEEK_SET) -> 14;  write "XY"      (hole [10..14), size 16)
 *   lseek(0, SEEK_END)  -> 16
 *   final file = "0123456789" + 4 zero bytes + "XY"
 *
 * The full 16-byte file checksums to sum('0'..'9') + 'X' + 'Y' = 525 + 177 =
 * 702 (the four hole bytes are zero) and the final size is 16. Only invariants
 * are printed:
 *
 *   lseek_positioning size=16 checksum=702 ok=11
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
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/* Read exactly n bytes at the current offset into buf (advances the offset). */
static void read_exact(int fd, char *buf, size_t n) {
  size_t got = 0;
  while (got < n) {
    ssize_t r = read(fd, buf + got, n - got);
    if (r < 0) {
      if (errno == EINTR)
        continue;
      fail("read");
    }
    if (r == 0)
      fail("read short (unexpected EOF)");
    got += (size_t)r;
  }
}

/* Read exactly n bytes at absolute offset off into buf (offset-independent). */
static void pread_exact(int fd, char *buf, size_t n, off_t off) {
  size_t got = 0;
  while (got < n) {
    ssize_t r = pread(fd, buf + got, n - got, off + (off_t)got);
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
  char template[] = "/tmp/lseek_positioning_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (unlink(template) != 0)
    fail("unlink");

  int ok = 0;

  if (write(fd, "0123456789", 10) != 10)
    fail("write 0123456789");

  /* SEEK_END with delta 0 reports the file size. */
  if (lseek(fd, 0, SEEK_END) == 10)
    ok++;

  /* SEEK_SET sets an absolute offset; the read returns those bytes. */
  if (lseek(fd, 3, SEEK_SET) == 3)
    ok++;
  char two[2];
  read_exact(fd, two, sizeof(two)); /* offset 3 -> 5 */
  if (memcmp(two, "34", 2) == 0)
    ok++;

  /* SEEK_CUR adds to the current offset (5 + 2 = 7). */
  if (lseek(fd, 2, SEEK_CUR) == 7)
    ok++;
  char one[1];
  read_exact(fd, one, sizeof(one)); /* offset 7 -> 8 */
  if (one[0] == '7')
    ok++;

  /* SEEK_END with a negative delta addresses the last byte (10 - 1 = 9). */
  if (lseek(fd, -1, SEEK_END) == 9)
    ok++;
  read_exact(fd, one, sizeof(one)); /* offset 9 -> 10 */
  if (one[0] == '9')
    ok++;

  /* Seek past EOF then write: a zero-filled hole spans [10..14). */
  if (lseek(fd, 14, SEEK_SET) == 14)
    ok++;
  if (write(fd, "XY", 2) != 2)
    fail("write XY");
  if (lseek(fd, 0, SEEK_END) == 16)
    ok++;

  char whole[16];
  pread_exact(fd, whole, sizeof(whole), 0);
  if (all_zero(whole, 10, 14))
    ok++;
  if (memcmp(whole + 14, "XY", 2) == 0)
    ok++;

  struct stat st;
  if (fstat(fd, &st) != 0)
    fail("fstat");
  long checksum = 0;
  for (size_t i = 0; i < sizeof(whole); i++)
    checksum += (unsigned char)whole[i];

  if (close(fd) != 0)
    fail("close");

  printf("lseek_positioning size=%ld checksum=%ld ok=%d\n", (long)st.st_size,
         checksum, ok);
  return 0;
}
