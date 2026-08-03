/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * statx(2) extended-metadata parity probe.
 *
 * A single process creates one temporary file and interrogates it with statx,
 * checking the deterministic metadata Detcore's file model must report
 * identically on every backend. Unlike the fstat-based file-metadata row, this
 * exercises the modern statx entry point and its request-mask/result-mask
 * contract, restricting observation to fields that are a pure function of the
 * file's own contents:
 *
 *   - statx(AT_FDCWD, path, ...) reports the requested STATX_SIZE and
 *     STATX_TYPE, with stx_size equal to the byte count written and a regular
 *     file type.
 *   - A freshly created regular file has link count one.
 *   - statx(fd, "", AT_EMPTY_PATH, ...) reports the same size via the open
 *     descriptor.
 *   - statx on a missing path fails deterministically with ENOENT.
 *
 * Over a file containing the six bytes "abcdef" the deterministic invariants are
 * a size of 6 and a content checksum of 'a'+'b'+'c'+'d'+'e'+'f' = 597. Only
 * invariants are printed:
 *
 *   statx_metadata size=6 checksum=597 ok=5
 *
 * It is deliberately free of gated concerns: single process, no fork/thread, and
 * no pid, timestamp, cpu-time, inode, device, uid, gid, or address is observed.
 */

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

int main(void) {
  char template[] = "/tmp/statx_metadata_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (write(fd, PAYLOAD, PAYLOAD_LEN) != PAYLOAD_LEN)
    fail("write payload");

  int ok = 0;
  struct statx stx;

  /* statx by path reports the requested size and regular-file type. */
  memset(&stx, 0, sizeof(stx));
  if (statx(AT_FDCWD, template, 0, STATX_SIZE | STATX_TYPE | STATX_NLINK,
            &stx) == 0 &&
      (stx.stx_mask & STATX_SIZE) != 0 && stx.stx_size == PAYLOAD_LEN)
    ok++;

  /* The mode field marks it as a regular file. */
  if ((stx.stx_mode & S_IFMT) == S_IFREG)
    ok++;

  /* A freshly created regular file has exactly one hard link. */
  if ((stx.stx_mask & STATX_NLINK) != 0 && stx.stx_nlink == 1)
    ok++;

  /* statx via the open descriptor with AT_EMPTY_PATH reports the same size. */
  memset(&stx, 0, sizeof(stx));
  if (statx(fd, "", AT_EMPTY_PATH, STATX_SIZE, &stx) == 0 &&
      stx.stx_size == PAYLOAD_LEN)
    ok++;

  /* statx on a missing path fails deterministically with ENOENT. */
  memset(&stx, 0, sizeof(stx));
  errno = 0;
  if (statx(AT_FDCWD, "/tmp/statx_metadata_absent_marker", 0, STATX_SIZE,
            &stx) < 0 &&
      errno == ENOENT)
    ok++;

  /* Read the payload back to derive a content checksum independent of statx. */
  char buf[PAYLOAD_LEN];
  if (pread(fd, buf, PAYLOAD_LEN, 0) != PAYLOAD_LEN)
    fail("pread payload");
  long checksum = 0;
  for (size_t i = 0; i < PAYLOAD_LEN; i++)
    checksum += (unsigned char)buf[i];
  long size = (long)PAYLOAD_LEN;

  if (close(fd) != 0)
    fail("close");
  if (unlink(template) != 0)
    fail("unlink");

  printf("statx_metadata size=%ld checksum=%ld ok=%d\n", size, checksum, ok);
  return 0;
}
