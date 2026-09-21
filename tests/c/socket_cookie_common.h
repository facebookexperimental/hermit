#ifndef HERMIT_TESTS_C_SOCKET_COOKIE_COMMON_H
#define HERMIT_TESTS_C_SOCKET_COOKIE_COMMON_H

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef SO_COOKIE
#define SO_COOKIE 57
#endif

static int read_socket_cookie(int fd, uint64_t* cookie) {
  *cookie = 0;
  socklen_t length = sizeof(*cookie);
  if (getsockopt(fd, SOL_SOCKET, SO_COOKIE, cookie, &length) != 0) {
    perror("getsockopt(SO_COOKIE)");
    return 1;
  }
  if (length != sizeof(*cookie) || *cookie == 0) {
    fprintf(
        stderr,
        "invalid SO_COOKIE result: length=%u cookie=%" PRIu64 "\n",
        length,
        *cookie);
    return 1;
  }
  return 0;
}

static int verify_socket_cookies(int first_fd, int second_fd, const char* label) {
  uint64_t first;
  uint64_t second;
  if (read_socket_cookie(first_fd, &first) != 0 ||
      read_socket_cookie(second_fd, &second) != 0) {
    return 1;
  }
  if (first == second) {
    fprintf(stderr, "SO_COOKIE reused live identity %" PRIu64 "\n", first);
    return 1;
  }

  const int alias_fd = dup(first_fd);
  if (alias_fd < 0) {
    perror("dup");
    return 1;
  }
  uint64_t alias;
  const int alias_result = read_socket_cookie(alias_fd, &alias);
  close(alias_fd);
  if (alias_result != 0) {
    return 1;
  }
  if (alias != first) {
    fprintf(
        stderr,
        "dup alias changed SO_COOKIE from %" PRIu64 " to %" PRIu64 "\n",
        first,
        alias);
    return 1;
  }

  printf("%s=%" PRIu64 ",%" PRIu64 "\n", label, first, second);
  return 0;
}

#endif
