/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * File-backed mmap(2) MAP_PRIVATE read and copy-on-write parity probe.
 *
 * A single process writes a fixed payload to one temporary file and maps it
 * MAP_PRIVATE, exercising the deterministic file-mapping semantics Detcore's
 * memory model must preserve identically on every backend:
 *
 *   - A MAP_PRIVATE mapping of a regular file exposes the file's bytes.
 *   - A write through a MAP_PRIVATE mapping is copy-on-write: the mapping sees
 *     the modified byte, but the underlying file is unchanged.
 *   - A fresh MAP_PRIVATE PROT_READ mapping still sees the original payload,
 *     proving the copy-on-write store never reached the file or a shared page.
 *
 * The payload is the 16 bytes "ABCDEFGHIJKLMNOP" (sum 1160). The sequence is:
 *   mmap PROT_READ|PROT_WRITE, MAP_PRIVATE -> content == payload
 *   store 'z' at offset 0                  -> mapping sees 'z', file unchanged
 *   munmap; pread the file                 -> still the original payload
 *   reopen O_RDONLY; mmap PROT_READ        -> content == payload again
 *
 * Only invariants are printed:
 *
 *   file_backed_mmap size=16 checksum=1160 ok=5
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
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#define PAYLOAD "ABCDEFGHIJKLMNOP"
#define PAYLOAD_LEN 16

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
  char template[] = "/tmp/file_backed_mmap_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (write(fd, PAYLOAD, PAYLOAD_LEN) != PAYLOAD_LEN)
    fail("write payload");

  int ok = 0;

  /* MAP_PRIVATE mapping exposes the file's bytes. */
  char *map = mmap(NULL, PAYLOAD_LEN, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
  if (map == MAP_FAILED)
    fail("mmap private");
  if (memcmp(map, PAYLOAD, PAYLOAD_LEN) == 0)
    ok++;

  /* A store through the mapping is copy-on-write and visible in the mapping. */
  map[0] = 'z';
  if (map[0] == 'z' && memcmp(map + 1, PAYLOAD + 1, PAYLOAD_LEN - 1) == 0)
    ok++;
  if (munmap(map, PAYLOAD_LEN) != 0)
    fail("munmap private");

  /* The copy-on-write store never reached the underlying file. */
  char buf[PAYLOAD_LEN];
  if (pread(fd, buf, PAYLOAD_LEN, 0) == PAYLOAD_LEN &&
      memcmp(buf, PAYLOAD, PAYLOAD_LEN) == 0)
    ok++;
  if (close(fd) != 0)
    fail("close");

  /* A fresh read-only mapping still sees the original payload. */
  int ro = open(template, O_RDONLY);
  if (ro < 0)
    fail("open ro");
  off_t final_size = fd_size(ro);
  char *view = mmap(NULL, PAYLOAD_LEN, PROT_READ, MAP_PRIVATE, ro, 0);
  if (view == MAP_FAILED)
    fail("mmap ro");
  long checksum = 0;
  for (size_t i = 0; i < PAYLOAD_LEN; i++)
    checksum += (unsigned char)view[i];
  if (memcmp(view, PAYLOAD, PAYLOAD_LEN) == 0)
    ok++;
  if (final_size == PAYLOAD_LEN)
    ok++;
  if (munmap(view, PAYLOAD_LEN) != 0)
    fail("munmap ro");
  if (close(ro) != 0)
    fail("close ro");

  if (unlink(template) != 0)
    fail("unlink");

  printf("file_backed_mmap size=%ld checksum=%ld ok=%d\n", (long)final_size,
         checksum, ok);
  return 0;
}
