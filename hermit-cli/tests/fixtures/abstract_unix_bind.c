/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * abstract_unix_bind: for each argument, in order, create an AF_UNIX stream
 * socket and bind it to that abstract name (a leading NUL, then the
 * argument's bytes). The argument "autobind" binds a bare family instead, so
 * the name is chosen for the program. Each bind prints "bound ARG" or
 * "bind ARG: ERRNO-NAME" on standard output; the sockets stay open until the
 * program exits 0.
 */

#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static const char* errno_name(int error) {
  switch (error) {
    case EADDRINUSE:
      return "EADDRINUSE";
    case EACCES:
      return "EACCES";
    case EINVAL:
      return "EINVAL";
    default:
      return "other";
  }
}

int main(int argc, char** argv) {
  for (int index = 1; index < argc; ++index) {
    const char* name = argv[index];
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
      perror("socket");
      return 2;
    }
    struct sockaddr_un address;
    memset(&address, 0, sizeof(address));
    address.sun_family = AF_UNIX;
    socklen_t length = offsetof(struct sockaddr_un, sun_path);
    if (strcmp(name, "autobind") != 0) {
      size_t name_length = strlen(name);
      if (name_length + 1 > sizeof(address.sun_path)) {
        fprintf(stderr, "name too long: %s\n", name);
        return 2;
      }
      memcpy(address.sun_path + 1, name, name_length);
      length += 1 + name_length;
    }
    if (bind(fd, (struct sockaddr*)&address, length) == 0) {
      printf("bound %s\n", name);
    } else {
      printf("bind %s: %s\n", name, errno_name(errno));
    }
  }
  return 0;
}
