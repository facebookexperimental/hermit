/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A process's cgroup file under Hermit
// (https://github.com/rrnewton/hermit/issues/4378).
//
// Usage: process_cgroup seek [PATH] | read PATH
//   seek  open PATH (/proc/self/cgroup by default) and try SEEK_END,
//         SEEK_DATA and SEEK_HOLE before reading it and again after one
//         read, printing each result
//         or its errno's name; then SEEK_SET 0 and print what a read returns.
//         Linux's seq_lseek gives EINVAL for all three, before and after.
//   read  print whether PATH is on procfs, then PATH's contents.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/vfs.h>
#include <unistd.h>

static void seeks(int fd, const char* when) {
  static const struct {
    int whence;
    const char* name;
  } modes[] = {{SEEK_END, "end"}, {SEEK_DATA, "data"}, {SEEK_HOLE, "hole"}};
  printf("%s:", when);
  for (unsigned i = 0; i < sizeof(modes) / sizeof(modes[0]); i++) {
    off_t result = lseek(fd, 0, modes[i].whence);
    if (result < 0) {
      printf(" %s %s", modes[i].name, errno == EINVAL ? "EINVAL" : strerror(errno));
    } else {
      printf(" %s %lld", modes[i].name, (long long)result);
    }
  }
  printf("\n");
}

int main(int argc, char** argv) {
  char buf[4096];
  if (argc >= 2 && !strcmp(argv[1], "seek")) {
    const char* path = argc >= 3 ? argv[2] : "/proc/self/cgroup";
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
      perror(path);
      return 1;
    }
    seeks(fd, "cold");
    if (read(fd, buf, sizeof(buf)) < 0) {
      perror("read");
      return 1;
    }
    seeks(fd, "read");
    if (lseek(fd, 0, SEEK_SET) != 0) {
      perror("lseek");
      return 1;
    }
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n < 0) {
      perror("read");
      return 1;
    }
    printf("again: %.*s", (int)n, buf);
    return 0;
  }
  if (argc >= 3 && !strcmp(argv[1], "read")) {
    int fd = open(argv[2], O_RDONLY);
    struct statfs fs;
    if (fd < 0 || fstatfs(fd, &fs) != 0) {
      perror(argv[2]);
      return 1;
    }
    printf("procfs %d\n", fs.f_type == 0x9fa0);
    ssize_t n;
    while ((n = read(fd, buf, sizeof(buf))) > 0) {
      fwrite(buf, 1, (size_t)n, stdout);
    }
    return n < 0;
  }
  fprintf(stderr, "usage: %s seek [PATH] | read PATH\n", argv[0]);
  return 64;
}
