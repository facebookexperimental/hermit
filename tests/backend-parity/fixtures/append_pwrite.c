/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Positional-write and O_APPEND parity probe.
 *
 * A single process opens one temporary file and exercises the offset semantics
 * that Detcore's file model must preserve identically on every backend:
 *
 *   - pwrite(2) writes at an explicit offset and does NOT move the descriptor's
 *     own file offset.
 *   - pread(2) reads at an explicit offset and does NOT move the descriptor's
 *     own file offset.
 *   - An ordinary write(2) uses and advances the descriptor's own offset.
 *   - After O_APPEND is enabled with fcntl(F_SETFL), every write lands at the
 *     current end of file regardless of the descriptor's offset.
 *
 * The sequence deterministically produces the file contents "XY2345Z":
 *   write "0123"          -> "0123"      (offset 4)
 *   pwrite "XY" @ 0       -> "XY23"      (offset unchanged: 4)
 *   write "45"            -> "XY2345"    (offset 6)
 *   pread 2 @ 2           -> reads "23"  (offset unchanged: 6)
 *   lseek to 0, O_APPEND, write "Z" -> "XY2345Z" (append ignores the offset)
 *
 * The bytes checksum to 'X'+'Y'+'2'+'3'+'4'+'5'+'Z' = 473 and the final size is
 * 7. Only invariants are printed:
 *
 *   append_pwrite size=7 checksum=473 ok=6
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

static off_t own_offset(int fd) {
  off_t off = lseek(fd, 0, SEEK_CUR);
  if (off < 0)
    fail("lseek SEEK_CUR");
  return off;
}

int main(void) {
  char template[] = "/tmp/append_pwrite_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (unlink(template) != 0)
    fail("unlink");

  int ok = 0;

  /* Ordinary write uses and advances the descriptor offset. */
  if (write(fd, "0123", 4) != 4)
    fail("write 0123");

  /* pwrite writes at an explicit offset without moving the descriptor offset. */
  if (pwrite(fd, "XY", 2, 0) != 2)
    fail("pwrite XY");
  if (own_offset(fd) == 4)
    ok++;

  /* The next ordinary write lands at the (unchanged) offset 4 and advances it. */
  if (write(fd, "45", 2) != 2)
    fail("write 45");
  if (own_offset(fd) == 6)
    ok++;

  /* pread reads at an explicit offset without moving the descriptor offset. */
  char two[2];
  if (pread(fd, two, 2, 2) != 2)
    fail("pread @2");
  if (memcmp(two, "23", 2) == 0)
    ok++;
  if (own_offset(fd) == 6)
    ok++;

  /* Enable O_APPEND: writes now land at end of file regardless of the offset. */
  int flags = fcntl(fd, F_GETFL);
  if (flags < 0)
    fail("fcntl F_GETFL");
  if (fcntl(fd, F_SETFL, flags | O_APPEND) != 0)
    fail("fcntl F_SETFL O_APPEND");
  if (lseek(fd, 0, SEEK_SET) != 0)
    fail("lseek rewind");
  if (write(fd, "Z", 1) != 1)
    fail("write Z");

  struct stat st;
  if (fstat(fd, &st) != 0)
    fail("fstat");
  if (st.st_size == 7)
    ok++;

  /* Read the whole file back and checksum it. */
  char all[7];
  if (pread(fd, all, sizeof(all), 0) != (ssize_t)sizeof(all))
    fail("pread all");
  long checksum = 0;
  for (size_t i = 0; i < sizeof(all); i++)
    checksum += (unsigned char)all[i];
  if (memcmp(all, "XY2345Z", 7) == 0)
    ok++;

  if (close(fd) != 0)
    fail("close");

  printf("append_pwrite size=%ld checksum=%ld ok=%d\n", (long)st.st_size,
         checksum, ok);
  return 0;
}
