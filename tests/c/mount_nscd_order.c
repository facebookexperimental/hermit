#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void copy_file(const char *path) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    fprintf(stderr, "open %s: %s\n", path, strerror(errno));
    exit(1);
  }

  char buffer[4096];
  for (;;) {
    ssize_t count = read(fd, buffer, sizeof(buffer));
    if (count < 0) {
      fprintf(stderr, "read %s: %s\n", path, strerror(errno));
      exit(1);
    }
    if (count == 0) {
      break;
    }
    if (write(STDOUT_FILENO, buffer, (size_t)count) != count) {
      perror("write");
      exit(1);
    }
  }
  if (close(fd) != 0) {
    perror("close");
    exit(1);
  }
}

int main(void) {
  copy_file("/var/run/nscd/from-later");
  int leaked = open("/run/nscd/from-var", O_RDONLY);
  if (leaked >= 0) {
    fprintf(stderr, "the /var source leaked through the protected /run/nscd mount\n");
    close(leaked);
    return 1;
  }
  if (errno != ENOENT) {
    fprintf(stderr, "open /run/nscd/from-var: %s\n", strerror(errno));
    return 1;
  }
  copy_file("/proc/self/mountinfo");
  return 0;
}
