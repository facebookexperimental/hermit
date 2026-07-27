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
  char numeric_path[64];
  int written = snprintf(numeric_path, sizeof(numeric_path), "/proc/%ld/fd/1",
                         (long)getpid());
  if (written < 0 || (size_t)written >= sizeof(numeric_path)) {
    return EXIT_FAILURE;
  }

  print_link("canonical", "/proc/self/fd/1", 128);
  print_link("truncated", "/proc/self/fd/1", 10);
  print_link("numeric", numeric_path, 128);
  print_link("dev-fd", "/dev/fd/1", 128);
  print_link("lexical", "/proc/self/fd/../fd/1", 128);

  int directory = open("/proc/self/fd", O_PATH | O_DIRECTORY);
  if (directory < 0) {
    perror("open proc fd directory");
    return EXIT_FAILURE;
  }
  char buffer[128];
  ssize_t count = readlinkat(directory, "1", buffer, sizeof(buffer));
  if (count < 0) {
    perror("readlinkat");
    return EXIT_FAILURE;
  }
  printf("readlinkat=%.*s\n", (int)count, buffer);
  return close(directory) == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
}
