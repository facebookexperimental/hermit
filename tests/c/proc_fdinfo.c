#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

#ifndef FD_SOURCE
#error "FD_SOURCE must select open (1), openat (2), or memfd_create (3)"
#endif

static int open_test_fd(void) {
#if FD_SOURCE == 1
  int fd = open("/tmp/hermit-proc-fdinfo-open", O_CREAT | O_RDWR, 0600);
  if (fd >= 0 && dprintf(fd, "x") < 0) {
    return -1;
  }
  return fd;
#elif FD_SOURCE == 2
  return openat(AT_FDCWD, "/tmp/hermit-proc-fdinfo-openat",
                O_CREAT | O_RDONLY, 0600);
#elif FD_SOURCE == 3
  return memfd_create("hermit-proc-fdinfo", MFD_CLOEXEC);
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

  FILE *info = fopen(path, "r");
  if (info == NULL) {
    return 1;
  }
  for (int ch; (ch = fgetc(info)) != EOF;) {
    putchar(ch);
  }
  return 0;
}
