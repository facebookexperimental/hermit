#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <unistd.h>

#ifndef FD_SOURCE
#error \
    "FD_SOURCE must select open (1), openat (2), memfd_create (3), eventfd (4), or pipe (5)"
#endif

static int open_test_fd(void) {
#if FD_SOURCE == 1
  int fd = open("/tmp/hermit-proc-fdinfo-open", O_CREAT | O_RDWR, 0600);
  if (fd >= 0 && dprintf(fd, "x") < 0) {
    return -1;
  }
  return fd;
#elif FD_SOURCE == 2
  return openat(
      AT_FDCWD, "/tmp/hermit-proc-fdinfo-openat", O_CREAT | O_RDONLY, 0600);
#elif FD_SOURCE == 3
  return memfd_create("hermit-proc-fdinfo", MFD_CLOEXEC);
#elif FD_SOURCE == 4
  // An eventfd whose count two writes raised, which its fdinfo shows
  // (`eventfd-count`), with its ID (`eventfd-id`).
  int fd = eventfd(3, EFD_CLOEXEC);
  uint64_t add = 2;
  if (fd >= 0 && write(fd, &add, sizeof(add)) != (ssize_t)sizeof(add)) {
    return -1;
  }
  return fd;
#elif FD_SOURCE == 5
  // A pipe's read end with a byte queued: its position stays 0.
  int fds[2];
  if (pipe2(fds, O_CLOEXEC) != 0 || write(fds[1], "x", 1) != 1) {
    return -1;
  }
  return fds[0];
#else
#error "unsupported FD_SOURCE"
#endif
}

int main(void) {
  int fd = open_test_fd();
  char path[64];
  if (fd < 0 || snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd) < 0) {
    return 1;
  }

  FILE* info = fopen(path, "r");
  if (info == NULL) {
    return 1;
  }
  for (int ch; (ch = fgetc(info)) != EOF;) {
    putchar(ch);
  }
  return 0;
}
