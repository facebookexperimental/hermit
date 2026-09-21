#define _GNU_SOURCE

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

static int write_all(int fd, const char *buf, size_t len) {
  while (len != 0) {
    ssize_t written = write(fd, buf, len);
    if (written < 0) {
      if (errno == EINTR)
        continue;
      return -1;
    }
    buf += written;
    len -= (size_t)written;
  }
  return 0;
}

static int request_complete(const char *buf, size_t len) {
  return (len >= 4 && memmem(buf, len, "\r\n\r\n", 4) != NULL) ||
         (len >= 2 && memmem(buf, len, "\n\n", 2) != NULL);
}

int main(int argc, char **argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s PORT_FILE RESPONSE_FILE\n", argv[0]);
    return 2;
  }

  int response_fd = open(argv[2], O_RDONLY | O_CLOEXEC);
  if (response_fd < 0) {
    perror("open response");
    return 1;
  }

  int listener = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (listener < 0) {
    perror("socket");
    return 1;
  }
  struct sockaddr_in address = {
      .sin_family = AF_INET,
      .sin_port = htons(0),
      .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
  };
  if (bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0) {
    perror("bind");
    return 1;
  }
  socklen_t address_len = sizeof(address);
  if (getsockname(listener, (struct sockaddr *)&address, &address_len) != 0) {
    perror("getsockname");
    return 1;
  }
  if (listen(listener, 1) != 0) {
    perror("listen");
    return 1;
  }

  size_t port_path_len = strlen(argv[1]) + sizeof(".tmp");
  char *port_path = malloc(port_path_len);
  if (port_path == NULL ||
      snprintf(port_path, port_path_len, "%s.tmp", argv[1]) < 0) {
    perror("prepare port path");
    return 1;
  }
  int port_fd = open(port_path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
  if (port_fd < 0) {
    perror("open port file");
    return 1;
  }
  char port[16];
  int port_len = snprintf(port, sizeof(port), "%u\n", ntohs(address.sin_port));
  if (port_len <= 0 || (size_t)port_len >= sizeof(port) ||
      write_all(port_fd, port, (size_t)port_len) != 0 || close(port_fd) != 0) {
    perror("write port file");
    return 1;
  }
  if (rename(port_path, argv[1]) != 0) {
    perror("publish port file");
    return 1;
  }
  free(port_path);

  int client;
  do {
    client = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
  } while (client < 0 && errno == EINTR);
  if (client < 0) {
    perror("accept");
    return 1;
  }
  close(listener);

  char request[8192];
  size_t request_len = 0;
  while (!request_complete(request, request_len)) {
    if (request_len == sizeof(request)) {
      fprintf(stderr, "HTTP request headers exceeded %zu bytes\n",
              sizeof(request));
      return 1;
    }
    ssize_t got =
        read(client, request + request_len, sizeof(request) - request_len);
    if (got < 0) {
      if (errno == EINTR)
        continue;
      perror("read request");
      return 1;
    }
    if (got == 0) {
      fprintf(stderr, "client closed before completing HTTP request headers\n");
      return 1;
    }
    request_len += (size_t)got;
  }

  char response[4096];
  for (;;) {
    ssize_t got = read(response_fd, response, sizeof(response));
    if (got < 0) {
      if (errno == EINTR)
        continue;
      perror("read response");
      return 1;
    }
    if (got == 0)
      break;
    if (write_all(client, response, (size_t)got) != 0) {
      perror("write response");
      return 1;
    }
  }

  if (shutdown(client, SHUT_WR) != 0) {
    perror("shutdown");
    return 1;
  }
  close(client);
  close(response_fd);
  return 0;
}
