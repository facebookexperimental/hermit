// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

// Prints what a guest can see of its environment, then execs itself once (with
// no argument) and the child prints the same. Each line names the image:
//   <image> env <entry>      every entry of environ, in order;
//   <image> procenv <entry>  every non-empty NUL-separated entry of
//                            /proc/self/environ.
// With the argument "child" it prints and exits without the exec.

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

extern char** environ;

int main(int argc, char** argv) {
  const char* image = argc > 1 ? "child" : "parent";
  static char buffer[1 << 16];
  size_t length = 0;
  int fd = open("/proc/self/environ", O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    perror("open /proc/self/environ");
    return 2;
  }
  for (;;) {
    ssize_t n = read(fd, buffer + length, sizeof(buffer) - 1 - length);
    if (n < 0) {
      perror("read /proc/self/environ");
      return 2;
    }
    if (n == 0)
      break;
    length += (size_t)n;
  }
  close(fd);

  for (char** entry = environ; *entry != NULL; ++entry)
    printf("%s env %s\n", image, *entry);
  for (size_t at = 0; at < length;) {
    size_t entry_length = strnlen(buffer + at, length - at);
    if (entry_length > 0)
      printf("%s procenv %.*s\n", image, (int)entry_length, buffer + at);
    at += entry_length + 1;
  }
  fflush(stdout);

  if (argc == 1) {
    char* child[] = {argv[0], "child", NULL};
    execve(argv[0], child, environ);
    perror("execve");
    return 2;
  }
  return 0;
}
