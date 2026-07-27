#ifndef HERMIT_TESTS_C_UNIX_AUTOBIND_COMMON_H
#define HERMIT_TESTS_C_UNIX_AUTOBIND_COMMON_H

#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static int run_unix_autobind_probe(int socket_type, const char* label) {
  const int fd = socket(AF_UNIX, socket_type, 0);
  if (fd < 0) {
    perror("socket");
    return 1;
  }

  struct sockaddr_un requested;
  memset(&requested, 0, sizeof(requested));
  requested.sun_family = AF_UNIX;
  if (bind(
          fd,
          (const struct sockaddr*)&requested,
          offsetof(struct sockaddr_un, sun_path)) != 0) {
    perror("bind");
    close(fd);
    return 1;
  }

  struct sockaddr_un observed;
  memset(&observed, 0, sizeof(observed));
  socklen_t observed_length = sizeof(observed);
  if (getsockname(fd, (struct sockaddr*)&observed, &observed_length) != 0) {
    perror("getsockname");
    close(fd);
    return 1;
  }
  close(fd);

  const socklen_t expected_length =
      offsetof(struct sockaddr_un, sun_path) + 6;
  if (observed.sun_family != AF_UNIX ||
      observed_length != expected_length || observed.sun_path[0] != '\0') {
    fprintf(
        stderr,
        "invalid autobind shape: family=%d length=%u first=%d\n",
        observed.sun_family,
        observed_length,
        observed.sun_path[0]);
    return 1;
  }
  for (size_t index = 1; index < 6; ++index) {
    const char byte = observed.sun_path[index];
    if (!((byte >= '0' && byte <= '9') || (byte >= 'a' && byte <= 'f'))) {
      fprintf(stderr, "invalid autobind hex byte at %zu: %d\n", index, byte);
      return 1;
    }
  }

  printf("%s=%.*s\n", label, 5, &observed.sun_path[1]);
  return 0;
}

#endif
