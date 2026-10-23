#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Make exactly argv[1] raw getpid calls, then print the count. */
int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s CALLS\n", argv[0]);
    return 2;
  }
  char *end = NULL;
  errno = 0;
  unsigned long long calls = strtoull(argv[1], &end, 10);
  if (errno != 0 || end == argv[1] || *end != '\0' || argv[1][0] == '-') {
    fprintf(stderr, "invalid call count: %s\n", argv[1]);
    return 2;
  }

  uint64_t completed = 0;
  for (unsigned long long call = 0; call < calls; ++call) {
    if (syscall(SYS_getpid) < 0) {
      perror("getpid");
      return errno == 0 ? 1 : errno;
    }
    ++completed;
  }

  printf("calls=%" PRIu64 "\n", completed);
  return 0;
}
