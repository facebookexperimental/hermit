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
//
// Every requested time is before 2038-01-19T03:14:07Z (2^31 - 1 seconds).
// Linux clamps a timestamp to the range the filesystem can store, and a
// filesystem without 64-bit timestamps (ext4 with 128-byte inodes, XFS without
// bigtime) stores at most 2^31 - 1. Asking such a filesystem for a later mtime
// makes Linux itself report 2147483647, not the request, so a post-2038 value
// would test the host's filesystem rather than Hermit. The values below are far
// from both the epoch and the present, so neither a canonical mtime nor a
// virtual write time can satisfy a check by accident.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define REQUESTED_ATIME 1111111111L  // 2005-03-18
#define FIRST_MTIME 1222222222L      // 2008-09-24
#define SECOND_MTIME 1333333333L     // 2012-04-02

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
  struct timespec ts[2] = {{REQUESTED_ATIME, 0}, {FIRST_MTIME, 0}};
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
  if (m == FIRST_MTIME && st.st_mtim.tv_nsec == 0)
    ok++;
  // (3) Determinized: atime does not echo the request (native keeps it).
  if (a != REQUESTED_ATIME)
    ok++;

  // Omit atime, request a fresh distinct mtime.
  struct timespec ts2[2] = {{0, UTIME_OMIT}, {SECOND_MTIME, 0}};
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
  if (m2 == SECOND_MTIME && a2 == a)
    ok++;

  close(fd);
  unlink(path);
  rmdir(dir);
  printf("utimensat ok=%d\n", ok);
  return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
