/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Path-based file-operation parity probe: truncate(2) and access(2)/faccessat(2).
 *
 * A single process drives one temporary path through operations that resolve
 * the file by name rather than by an open descriptor, checking the deterministic
 * semantics Detcore's file model must preserve identically on every backend.
 * This complements the fd-based ftruncate row (which truncates through an open
 * descriptor) by exercising the path-resolving variants:
 *
 *   - truncate(path, N) shrinks the file to N bytes.
 *   - truncate(path, M) with M > N grows it, zero-filling the new hole.
 *   - access(path, ...) and faccessat(AT_FDCWD, path, ...) confirm existence
 *     and R_OK/W_OK permission for an existing path.
 *   - access() on a missing path fails deterministically with ENOENT.
 *
 * The sequence over an initial file containing "HELLOWORLD" (10 bytes) is:
 *   truncate(path, 4)      -> size 4 ("HELL")
 *   truncate(path, 8)      -> size 8 ("HELL\0\0\0\0")
 *   read back              -> "HELL" then four zero bytes
 *   access existing        -> F_OK and R_OK|W_OK succeed
 *   faccessat existing     -> R_OK succeeds
 *   access missing         -> -1 / ENOENT
 *
 * The eight retained bytes checksum to 'H'+'E'+'L'+'L' = 293 and the final size
 * is 8. Only invariants are printed:
 *
 *   path_file_ops size=8 checksum=293 ok=6
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

static off_t path_size(const char *path) {
  struct stat st;
  if (stat(path, &st) != 0)
    fail("stat");
  return st.st_size;
}

int main(void) {
  char template[] = "/tmp/path_file_ops_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (write(fd, "HELLOWORLD", 10) != 10)
    fail("write payload");
  if (close(fd) != 0)
    fail("close seed");

  int ok = 0;

  /* truncate(path, N) shrinks the file by name. */
  if (truncate(template, 4) == 0 && path_size(template) == 4)
    ok++;

  /* truncate(path, M) with M > N grows the file, zero-filling the hole. */
  if (truncate(template, 8) == 0 && path_size(template) == 8)
    ok++;

  /* The retained prefix survives and the grown region reads back as zeros. */
  int rd = open(template, O_RDONLY);
  if (rd < 0)
    fail("open readback");
  char buf[8];
  ssize_t n = pread(rd, buf, sizeof(buf), 0);
  if (close(rd) != 0)
    fail("close readback");
  if (n == 8 && memcmp(buf, "HELL", 4) == 0 && buf[4] == 0 && buf[5] == 0 &&
      buf[6] == 0 && buf[7] == 0)
    ok++;

  /* access() confirms existence and read/write permission for the path. */
  if (access(template, F_OK) == 0 && access(template, R_OK | W_OK) == 0)
    ok++;

  /* faccessat() resolves the same path relative to the current directory. */
  if (faccessat(AT_FDCWD, template, R_OK, 0) == 0)
    ok++;

  /* access() on a missing path fails deterministically with ENOENT. */
  errno = 0;
  if (access("/tmp/path_file_ops_absent_marker", F_OK) < 0 && errno == ENOENT)
    ok++;

  off_t final_size = path_size(template);
  long checksum = 0;
  for (size_t i = 0; i < 4; i++)
    checksum += (unsigned char)buf[i];

  if (unlink(template) != 0)
    fail("unlink");

  printf("path_file_ops size=%ld checksum=%ld ok=%d\n", (long)final_size,
         checksum, ok);
  return 0;
}
