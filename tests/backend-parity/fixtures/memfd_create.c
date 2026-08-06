// Backend-parity contract: memfd_create anonymous file semantics.
//
// Exercises the anonymous in-memory file descriptor created by memfd_create(2):
// the close-on-exec flag, growth by write, positional read-back, and
// ftruncate-driven tail zero-fill. No blocking I/O and no host filesystem
// state, so it is deterministic and portable.
//
// _GNU_SOURCE is supplied by the harness compile flags (see run_matrix.py);
// do not define it here (it would collide with -D_GNU_SOURCE under -Werror).
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

// Report the close-on-exec descriptor flag for fd, or -1 on failure.
static int fd_cloexec(int fd) {
  int flags = fcntl(fd, F_GETFD);
  if (flags < 0) {
    return -1;
  }
  return (flags & FD_CLOEXEC) ? 1 : 0;
}

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

  int fd = memfd_create("parity", MFD_CLOEXEC);
  if (fd < 0) {
    printf("memfd_create ok=%d\n", ok);
    return 0;
  }
  ok++; // 1: descriptor created

  if (fd_cloexec(fd) == 1) {
    ok++; // 2: MFD_CLOEXEC set FD_CLOEXEC
  }

  const char payload[] = "abcdef";
  const size_t payload_len = sizeof payload - 1; // 6, no NUL
  if (write(fd, payload, payload_len) == (ssize_t)payload_len &&
      fd_size(fd) == (long)payload_len) {
    ok++; // 3: write grows the anonymous file
  }

  char buf[8] = {0};
  if (pread(fd, buf, payload_len, 0) == (ssize_t)payload_len &&
      memcmp(buf, payload, payload_len) == 0) {
    ok++; // 4: positional read-back matches
  }

  if (ftruncate(fd, 10) == 0 && fd_size(fd) == 10) {
    char tail[4] = {1, 1, 1, 1};
    if (pread(fd, tail, sizeof tail, (off_t)payload_len) == (ssize_t)sizeof tail &&
        tail[0] == 0 && tail[1] == 0 && tail[2] == 0 && tail[3] == 0) {
      ok++; // 5: ftruncate grows with zero-filled tail
    }
  }

  close(fd);
  printf("memfd_create ok=%d\n", ok);
  return 0;
}
