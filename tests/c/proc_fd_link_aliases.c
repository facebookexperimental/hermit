/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

static void print_link(const char *label, const char *path, size_t capacity) {
  char buffer[128];
  ssize_t count = readlink(path, buffer, capacity);
  if (count < 0) {
    perror(label);
    exit(EXIT_FAILURE);
  }
  printf("%s=%.*s\n", label, (int)count, buffer);
}

int main(void) {
  const char *target = "/tmp/hermit-proc-fd-link-aliases";
  int target_fd = open(target, O_CREAT | O_RDWR | O_TRUNC, 0600);
  if (target_fd < 0) {
    perror("open target");
    return EXIT_FAILURE;
  }

  char canonical_path[64];
  char numeric_path[64];
  char dev_fd_path[64];
  char lexical_path[64];
  char fd_name[16];
  int canonical_written = snprintf(canonical_path, sizeof(canonical_path),
                                   "/proc/self/fd/%d", target_fd);
  int numeric_written = snprintf(numeric_path, sizeof(numeric_path),
                                 "/proc/%ld/fd/%d", (long)getpid(), target_fd);
  int dev_fd_written =
      snprintf(dev_fd_path, sizeof(dev_fd_path), "/dev/fd/%d", target_fd);
  int lexical_written = snprintf(lexical_path, sizeof(lexical_path),
                                 "/proc/self/fd/../fd/%d", target_fd);
  int fd_name_written = snprintf(fd_name, sizeof(fd_name), "%d", target_fd);
  if (canonical_written < 0 ||
      (size_t)canonical_written >= sizeof(canonical_path) ||
      numeric_written < 0 || (size_t)numeric_written >= sizeof(numeric_path) ||
      dev_fd_written < 0 || (size_t)dev_fd_written >= sizeof(dev_fd_path) ||
      lexical_written < 0 || (size_t)lexical_written >= sizeof(lexical_path) ||
      fd_name_written < 0 || (size_t)fd_name_written >= sizeof(fd_name)) {
    return EXIT_FAILURE;
  }

  print_link("canonical", canonical_path, 128);
  print_link("truncated", canonical_path, 10);
  print_link("numeric", numeric_path, 128);
  print_link("dev-fd", dev_fd_path, 128);
  print_link("lexical", lexical_path, 128);

  int directory = open("/proc/self/fd", O_PATH | O_DIRECTORY);
  if (directory < 0) {
    perror("open proc fd directory");
    return EXIT_FAILURE;
  }
  char buffer[128];
  ssize_t count = readlinkat(directory, fd_name, buffer, sizeof(buffer));
  if (count < 0) {
    perror("readlinkat");
    return EXIT_FAILURE;
  }
  printf("readlinkat=%.*s\n", (int)count, buffer);
  int result = close(directory) | close(target_fd) | unlink(target);
  return result == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
}
