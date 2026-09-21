/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <stdio.h>
#include <string.h>

static int print_fdinfo(const char *name, int fd) {
  char path[64];
  if (snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd) < 0) {
    return 1;
  }
  FILE *file = fopen(path, "r");
  if (file == NULL) {
    return 1;
  }
  printf("[%s]\n", name);
  for (int byte; (byte = fgetc(file)) != EOF;) {
    putchar(byte);
  }
  return fclose(file) != 0;
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "--mount-namespace-only") == 0) {
    int namespace_fd = open("/proc/self/ns/mnt", O_RDONLY | O_CLOEXEC);
    return namespace_fd < 0 || print_fdinfo("mount-namespace", namespace_fd);
  }

  int pipe_fds[2];
  int socket_fds[2];
  int event_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  int pid_fd = syscall(SYS_pidfd_open, getpid(), 0);
  int pid_fd_errno = errno;
  int namespace_fd = open("/proc/self/ns/mnt", O_RDONLY | O_CLOEXEC);
  if (pipe(pipe_fds) != 0 ||
      socketpair(AF_UNIX, SOCK_STREAM, 0, socket_fds) != 0 || event_fd < 0 ||
      namespace_fd < 0) {
    return 1;
  }

  int result = print_fdinfo("pipe", pipe_fds[0]) ||
               print_fdinfo("socket", socket_fds[0]) ||
               print_fdinfo("eventfd", event_fd) ||
               print_fdinfo("mount-namespace", namespace_fd);
  if (pid_fd >= 0) {
    result = result || print_fdinfo("pidfd", pid_fd);
  } else if (pid_fd_errno != ENOSYS) {
    return 1;
  }
  return result;
}
