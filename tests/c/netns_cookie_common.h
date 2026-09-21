#ifndef HERMIT_TESTS_C_NETNS_COOKIE_COMMON_H
#define HERMIT_TESTS_C_NETNS_COOKIE_COMMON_H

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef SO_NETNS_COOKIE
#define SO_NETNS_COOKIE 71
#endif

static int read_netns_cookie(int fd, uint64_t* cookie) {
  *cookie = 0;
  socklen_t length = sizeof(*cookie);
  if (getsockopt(fd, SOL_SOCKET, SO_NETNS_COOKIE, cookie, &length) != 0) {
    perror("getsockopt(SO_NETNS_COOKIE)");
    return 1;
  }
  if (length != sizeof(*cookie) || *cookie == 0) {
    fprintf(
        stderr,
        "invalid SO_NETNS_COOKIE result: length=%u cookie=%" PRIu64 "\n",
        length,
        *cookie);
    return 1;
  }
  return 0;
}

static int verify_netns_cookie(int first_fd, int second_fd, const char* label) {
  uint64_t first;
  uint64_t second;
  if (read_netns_cookie(first_fd, &first) != 0 ||
      read_netns_cookie(second_fd, &second) != 0) {
    return 1;
  }
  if (first != second) {
    fprintf(
        stderr,
        "sockets in one network namespace had cookies %" PRIu64
        " and %" PRIu64 "\n",
        first,
        second);
    return 1;
  }

  printf("%s=%" PRIu64 "\n", label, first);
  return 0;
}

#endif
