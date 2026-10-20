/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// utimensat(2) file-timestamp determinization parity probe.
//
// A file's stored timestamps are host-derived state, so Hermit's stat reports
// virtual ones: the mtime moves with virtual time on writes and atime is a
// deterministic constant. An explicit mtime passed to utimensat is not host
// state, though. The guest chose it, so it is as deterministic as the program,
// and Linux reports it back exactly. `tar`, `cp -p`, `touch -r` and `make`
// depend on that (https://github.com/rrnewton/hermit/issues/3565). This file
// used to assert the opposite, that the requested mtime was overridden, which
// pinned that defect as policy.
//
// The checks never hard-code Hermit's internal time base. Under Hermit all five
// pass (ok=5). Native passes four (ok=4): it also echoes the requested atime,
// which Hermit keeps virtual.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

int main(void) {
  enum { EXPECTED_CHECKS = 5 };
  char dir[] = "/tmp/utimensat_determinism.XXXXXX";
  if (!mkdtemp(dir)) {
    printf("utimensat MKDTEMP_FAIL\n");
    return 1;
  }
  char path[256];
  snprintf(path, sizeof(path), "%s/f", dir);
  int fd = open(path, O_CREAT | O_RDWR, 0600);
  if (fd < 0) {
    printf("utimensat OPEN_FAIL\n");
    return 1;
  }

  int ok = 0;

  // Request two distinct explicit timestamps.
  struct timespec ts[2] = {{1111111111, 0}, {2222222222, 0}};
  // (1) utimensat is accepted (native and all backends).
  if (utimensat(AT_FDCWD, path, ts, 0) == 0)
    ok++;

  struct stat st;
  if (fstat(fd, &st) != 0) {
    close(fd);
    unlink(path);
    rmdir(dir);
    printf("utimensat FSTAT1_FAIL\n");
    return EXIT_FAILURE;
  }
  long a = st.st_atim.tv_sec;
  long m = st.st_mtim.tv_sec;
  // (2) The requested mtime is reported exactly, as on Linux.
  if (m == 2222222222L && st.st_mtim.tv_nsec == 0)
    ok++;
  // (3) Determinized: atime does not echo the request (native keeps it).
  if (a != 1111111111L)
    ok++;

  // Omit atime, request a fresh distinct mtime.
  struct timespec ts2[2] = {{0, UTIME_OMIT}, {3333333333, 0}};
  // (4) utimensat with UTIME_OMIT is accepted.
  if (utimensat(AT_FDCWD, path, ts2, 0) == 0)
    ok++;

  struct stat st2;
  if (fstat(fd, &st2) != 0) {
    close(fd);
    unlink(path);
    rmdir(dir);
    printf("utimensat FSTAT2_FAIL\n");
    return EXIT_FAILURE;
  }
  long a2 = st2.st_atim.tv_sec;
  long m2 = st2.st_mtim.tv_sec;
  // (5) The new mtime lands and the omitted atime is unchanged.
  if (m2 == 3333333333L && a2 == a)
    ok++;

  close(fd);
  unlink(path);
  rmdir(dir);
  printf("utimensat ok=%d\n", ok);
  return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
