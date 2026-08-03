// Backend-parity contract: fallocate(2) preallocation and hole punching.
//
// Exercises fallocate(2) on a self-created mkstemp file: default-mode
// preallocation that extends the file size, FALLOC_FL_PUNCH_HOLE |
// FALLOC_FL_KEEP_SIZE which zero-fills a range while preserving the size, and
// FALLOC_FL_KEEP_SIZE preallocation that reserves space without growing the
// file. All observations are content- and size-derived, and the file is
// removed before exit, so the contract is deterministic and portable.
//
// _GNU_SOURCE is supplied by the harness compile flags (see run_matrix.py);
// do not define it here (it would collide with -D_GNU_SOURCE under -Werror).
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

// Report the current size of fd via fstat, or -1 on failure.
static long fd_size(int fd) {
  struct stat st;
  if (fstat(fd, &st) != 0) {
    return -1;
  }
  return (long)st.st_size;
}

int main(void) {
  int ok = 0;

  char path[] = "/tmp/fallocXXXXXX";
  int fd = mkstemp(path);
  if (fd < 0) {
    printf("fallocate ok=%d\n", ok);
    return 0;
  }

  // 1: default-mode fallocate extends the file to offset + len.
  if (fallocate(fd, 0, 0, 8192) == 0 && fd_size(fd) == 8192) {
    ok++;
  }

  // 2: write a known non-zero pattern over the first 4 KiB.
  char pattern[4096];
  memset(pattern, 'A', sizeof pattern);
  if (pwrite(fd, pattern, sizeof pattern, 0) == (ssize_t)sizeof pattern) {
    ok++;
  }

  // 3: punch a hole over [0, 4096) with KEEP_SIZE; size is unchanged.
  if (fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, 4096) == 0 &&
      fd_size(fd) == 8192) {
    ok++;
  }

  // 4: the punched range now reads back as zeros.
  char readback[4096];
  memset(readback, 'X', sizeof readback);
  if (pread(fd, readback, sizeof readback, 0) == (ssize_t)sizeof readback) {
    int all_zero = 1;
    for (size_t i = 0; i < sizeof readback; i++) {
      if (readback[i] != 0) {
        all_zero = 0;
        break;
      }
    }
    if (all_zero) {
      ok++;
    }
  }

  // 5: KEEP_SIZE preallocation past EOF reserves space without growing size.
  if (fallocate(fd, FALLOC_FL_KEEP_SIZE, 8192, 4096) == 0 &&
      fd_size(fd) == 8192) {
    ok++;
  }

  close(fd);
  unlink(path);
  printf("fallocate ok=%d\n", ok);
  return 0;
}
