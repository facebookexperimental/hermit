#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#ifndef LOCK_API
#error "LOCK_API must select fcntl (1), lockf (2), or OFD fcntl (3)"
#endif

static int take_lock(int fd) {
#if LOCK_API == 1
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  return fcntl(fd, F_SETLK, &lock);
#elif LOCK_API == 2
  return lockf(fd, F_TLOCK, 0);
#elif LOCK_API == 3
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  return fcntl(fd, F_OFD_SETLK, &lock);
#else
#error "unsupported LOCK_API"
#endif
}

int main(void) {
  int fd = open("/tmp/hermit-proc-locks", O_CREAT | O_RDWR, 0600);
  if (fd < 0 || take_lock(fd) < 0) {
    return 1;
  }

  FILE *locks = fopen("/proc/locks", "r");
  if (locks == NULL) {
    return 1;
  }
  for (int ch; (ch = fgetc(locks)) != EOF;) {
    putchar(ch);
  }
  return 0;
}
