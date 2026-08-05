/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * File-description shared-offset parity probe.
 *
 * A single process checks the invariant Detcore's file model must preserve
 * identically on every backend: a descriptor produced by dup(2) shares the
 * same open file description as its origin, so the two descriptors share one
 * file offset, whereas a descriptor from an independent open(2) of the same
 * file has its own offset.
 *
 * One temporary file is created with contents "abcdefgh". Then:
 *   fd2 = dup(fd)                 -- fd2 and fd share one offset
 *   fd3 = open(path, O_RDWR)      -- fd3 has an independent offset
 *   unlink(path)                  -- all three descriptors stay open
 *
 * The checks are:
 *   lseek(fd, 2, SEEK_SET) -> 2
 *   read(fd2, 2) == "cd"                 (fd2 sees fd's offset 2)
 *   lseek(fd, 0, SEEK_CUR) == 4          (fd advanced by fd2's read)
 *   read(fd3, 4) == "abcd"               (fd3 starts at its own offset 0)
 *   lseek(fd3,0,SEEK_CUR)==4 && lseek(fd,0,SEEK_CUR)==4   (fd3 independent)
 *   write(fd2, "WXYZ", 4) at shared offset 4 -> file "abcdWXYZ"
 *
 * The final eight bytes checksum to 'a'+'b'+'c'+'d'+'W'+'X'+'Y'+'Z' = 748 and
 * the final size is 8. Only invariants are printed:
 *
 *   dup_shared_offset size=8 checksum=748 ok=6
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

int main(void) {
  char template[] = "/tmp/dup_shared_offset_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");

  if (write(fd, "abcdefgh", 8) != 8)
    fail("write abcdefgh");

  int fd2 = dup(fd);
  if (fd2 < 0)
    fail("dup");
  int fd3 = open(template, O_RDWR);
  if (fd3 < 0)
    fail("open independent");
  if (unlink(template) != 0)
    fail("unlink");

  int ok = 0;

  /* dup shares the open file description: fd2 sees fd's offset. */
  if (lseek(fd, 2, SEEK_SET) == 2)
    ok++;
  char two[2];
  read_exact(fd2, two, sizeof(two)); /* shared offset 2 -> 4 */
  if (memcmp(two, "cd", 2) == 0)
    ok++;
  /* The read through fd2 advanced the shared offset visible via fd. */
  if (lseek(fd, 0, SEEK_CUR) == 4)
    ok++;

  /* Independent open has its own offset, starting at 0. */
  char four[4];
  read_exact(fd3, four, sizeof(four)); /* fd3 offset 0 -> 4 */
  if (memcmp(four, "abcd", 4) == 0)
    ok++;
  /* fd3's read did not disturb the shared fd/fd2 offset. */
  if (lseek(fd3, 0, SEEK_CUR) == 4 && lseek(fd, 0, SEEK_CUR) == 4)
    ok++;

  /* A write through fd2 lands at the shared offset and is visible everywhere. */
  if (lseek(fd2, 4, SEEK_SET) != 4)
    fail("lseek fd2 to 4");
  if (write(fd2, "WXYZ", 4) != 4)
    fail("write WXYZ");
  char whole[8];
  pread_exact(fd, whole, sizeof(whole), 0);
  if (memcmp(whole, "abcdWXYZ", 8) == 0)
    ok++;

  struct stat st;
  if (fstat(fd, &st) != 0)
    fail("fstat");
  long checksum = 0;
  for (size_t i = 0; i < sizeof(whole); i++)
    checksum += (unsigned char)whole[i];

  if (close(fd) != 0 || close(fd2) != 0 || close(fd3) != 0)
    fail("close");

  printf("dup_shared_offset size=%ld checksum=%ld ok=%d\n", (long)st.st_size,
         checksum, ok);
  return 0;
}
