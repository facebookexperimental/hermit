// Backend-parity contract: flock(2) advisory-lock lifecycle on one descriptor.
//
// Exercises the flock(2) operation sequence on a single open file description
// of a self-created temporary file: exclusive acquire, downgrade to shared,
// release, non-blocking re-acquire of the now-free lock, and final release.
// Every operation returns 0 identically across all three backends.
//
// This fixture deliberately does NOT assert cross-descriptor contention.
// Detcore serializes guest execution and does not model flock contention
// between two open file descriptions the same way the host kernel does (the
// host returns EWOULDBLOCK on a conflicting non-blocking lock; Hermit does
// not), so a contention check is not a portable cross-backend contract. Only
// the single-descriptor lifecycle, which all backends reproduce identically,
// is asserted here. The temporary file is removed before exit.
//
// _GNU_SOURCE is supplied by the harness compile flags (see run_matrix.py);
// do not define it here (it would collide with -D_GNU_SOURCE under -Werror).
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/file.h>
#include <unistd.h>

int main(void) {
  int ok = 0;

  char path[] = "/tmp/flockXXXXXX";
  int fd = mkstemp(path);
  if (fd < 0) {
    printf("flock ok=%d\n", ok);
    return 0;
  }

  if (flock(fd, LOCK_EX) == 0) {
    ok++; // 1: exclusive acquire
  }
  if (flock(fd, LOCK_SH) == 0) {
    ok++; // 2: downgrade to shared
  }
  if (flock(fd, LOCK_UN) == 0) {
    ok++; // 3: release
  }
  if (flock(fd, LOCK_EX | LOCK_NB) == 0) {
    ok++; // 4: non-blocking acquire on the now-free lock
  }
  if (flock(fd, LOCK_UN) == 0) {
    ok++; // 5: final release
  }

  close(fd);
  unlink(path);
  printf("flock ok=%d\n", ok);
  return 0;
}
