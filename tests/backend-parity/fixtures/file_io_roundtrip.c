/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * File-IO sequence: create, write, fsync, lseek, read-back, stat, truncate.
 *
 * Builds a file in the guest's own tmp directory, writes a fixed byte stream,
 * fsyncs it, rewinds with lseek, reads it back and checksums it, stats the size
 * through both fstat and lseek(SEEK_END), truncates to half, re-stats, and
 * unlinks. Every step's result folds into one aggregate line.
 *
 * This is the ordinary file lifecycle a real program performs, as one sequence
 * rather than as isolated syscall probes: the existing coverage has separate
 * fixtures for mmap-backed files, sendfile, and copy_file_range, but none walks
 * write -> durability -> reposition -> read-back -> size -> shrink on a single
 * descriptor, which is where an fd-offset or size-cache divergence appears.
 *
 * Deterministic by construction: the path is fixed, the content is generated,
 * and the observables are a checksum and byte counts. No pid, timestamp, inode,
 * or address is observed -- note the filename deliberately omits getpid() so
 * that the output cannot vary between runs.
 */

/* The e2e harness compiles with -std=c11, which hides POSIX declarations. */
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define PAYLOAD 1024
#define PATH "/tmp/hermit_file_io_roundtrip.bin"

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/*
 * The e2e harness has no golden-output field: its verify oracle is exit status
 * plus cross-attempt determinism. A deterministically wrong stdout therefore
 * passes unnoticed unless the guest checks itself, so every invariant below is
 * asserted rather than merely printed.
 */
static int violations;

static void expect(const char *name, long long observed, long long wanted) {
  if (observed != wanted) {
    fprintf(stderr, "invariant %s: observed %lld, wanted %lld\n", name, observed,
            wanted);
    violations++;
  }
}

int main(void) {
  uint8_t payload[PAYLOAD];
  for (int i = 0; i < PAYLOAD; ++i)
    payload[i] = (uint8_t)(i * 7 + 3);

  int fd = open(PATH, O_RDWR | O_CREAT | O_TRUNC, 0600);
  if (fd < 0)
    fail("open");

  size_t written = 0;
  while (written < sizeof(payload)) {
    ssize_t n = write(fd, payload + written, sizeof(payload) - written);
    if (n < 0) {
      if (errno == EINTR)
        continue;
      fail("write");
    }
    written += (size_t)n;
  }
  if (fsync(fd) != 0)
    fail("fsync");

  off_t end_off = lseek(fd, 0, SEEK_END);
  if (end_off < 0)
    fail("lseek end");
  if (lseek(fd, 0, SEEK_SET) != 0)
    fail("lseek set");

  unsigned long checksum = 0;
  size_t read_bytes = 0;
  uint8_t buffer[128];
  for (;;) {
    ssize_t n = read(fd, buffer, sizeof(buffer));
    if (n < 0) {
      if (errno == EINTR)
        continue;
      fail("read");
    }
    if (n == 0)
      break;
    for (ssize_t i = 0; i < n; ++i)
      checksum += buffer[i];
    read_bytes += (size_t)n;
  }

  struct stat st;
  if (fstat(fd, &st) != 0)
    fail("fstat");
  off_t stat_size = st.st_size;

  if (ftruncate(fd, PAYLOAD / 2) != 0)
    fail("ftruncate");
  if (fstat(fd, &st) != 0)
    fail("fstat after truncate");
  off_t shrunk = st.st_size;

  if (close(fd) != 0)
    fail("close");
  if (unlink(PATH) != 0)
    fail("unlink");

  expect("wrote", (long long)written, 1024);
  expect("read", (long long)read_bytes, 1024);
  expect("checksum", (long long)checksum, 130560);
  expect("end", (long long)end_off, 1024);
  expect("stat", (long long)stat_size, 1024);
  expect("shrunk", (long long)shrunk, 512);
  printf("fileio wrote=%zu read=%zu checksum=%lu end=%lld stat=%lld shrunk=%lld\n",
         written, read_bytes, checksum, (long long)end_off, (long long)stat_size,
         (long long)shrunk);
  return violations == 0 ? 0 : 1;
}
