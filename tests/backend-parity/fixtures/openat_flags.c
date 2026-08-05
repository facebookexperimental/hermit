/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * open(2)/openat(2) flag-semantics parity probe.
 *
 * A single process drives one temporary path through a fixed sequence of opens
 * and checks the deterministic flag semantics Detcore's file model must preserve
 * identically on every backend:
 *
 *   - O_CREAT|O_EXCL on an existing path fails with EEXIST.
 *   - An O_WRONLY descriptor rejects read(2) with EBADF.
 *   - An O_RDONLY descriptor rejects write(2) with EBADF and reads the content.
 *   - O_TRUNC truncates an existing file to zero length on open.
 *
 * The sequence over an initial file containing "hello" is:
 *   open O_CREAT|O_EXCL  -> -1 / EEXIST
 *   open O_WRONLY; read  -> -1 / EBADF
 *   open O_RDONLY; write -> -1 / EBADF; read -> "hello"
 *   open O_WRONLY|O_TRUNC -> size 0; write "hi"
 *   open O_RDONLY        -> content "hi", size 2
 *
 * The final two bytes "hi" checksum to 'h'+'i' = 209 and the final size is 2.
 * Only invariants are printed:
 *
 *   openat_flags size=2 checksum=209 ok=6
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

static off_t fd_size(int fd) {
  struct stat st;
  if (fstat(fd, &st) != 0)
    fail("fstat");
  return st.st_size;
}

int main(void) {
  char template[] = "/tmp/openat_flags_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (write(fd, "hello", 5) != 5)
    fail("write hello");
  if (close(fd) != 0)
    fail("close seed");

  int ok = 0;
  char buf[8];

  /* O_CREAT|O_EXCL on an existing path must fail with EEXIST. */
  errno = 0;
  int excl = open(template, O_CREAT | O_EXCL | O_WRONLY, 0600);
  if (excl < 0 && errno == EEXIST)
    ok++;
  else if (excl >= 0)
    close(excl);

  /* A write-only descriptor rejects read with EBADF. */
  int wo = open(template, O_WRONLY);
  if (wo < 0)
    fail("open O_WRONLY");
  errno = 0;
  if (read(wo, buf, sizeof(buf)) < 0 && errno == EBADF)
    ok++;
  if (close(wo) != 0)
    fail("close wo");

  /* A read-only descriptor rejects write with EBADF and reads the content. */
  int ro = open(template, O_RDONLY);
  if (ro < 0)
    fail("open O_RDONLY");
  errno = 0;
  if (write(ro, "X", 1) < 0 && errno == EBADF)
    ok++;
  if (pread(ro, buf, 5, 0) == 5 && memcmp(buf, "hello", 5) == 0)
    ok++;
  if (close(ro) != 0)
    fail("close ro");

  /* O_TRUNC truncates the existing file to zero length on open. */
  int tr = open(template, O_WRONLY | O_TRUNC);
  if (tr < 0)
    fail("open O_TRUNC");
  if (fd_size(tr) == 0)
    ok++;
  if (write(tr, "hi", 2) != 2)
    fail("write hi");
  if (close(tr) != 0)
    fail("close tr");

  /* Reopen read-only and confirm the truncated-then-rewritten content. */
  int fin = open(template, O_RDONLY);
  if (fin < 0)
    fail("open final");
  char last[2];
  if (pread(fin, last, sizeof(last), 0) == 2 && memcmp(last, "hi", 2) == 0 &&
      fd_size(fin) == 2)
    ok++;
  off_t final_size = fd_size(fin);
  long checksum = 0;
  for (size_t i = 0; i < sizeof(last); i++)
    checksum += (unsigned char)last[i];
  if (close(fin) != 0)
    fail("close final");

  if (unlink(template) != 0)
    fail("unlink");

  printf("openat_flags size=%ld checksum=%ld ok=%d\n", (long)final_size,
         checksum, ok);
  return 0;
}
