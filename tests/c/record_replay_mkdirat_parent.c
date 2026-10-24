/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Successful mkdirat calls beneath directories that exist on the host at
 * record time, which a standalone replay's empty chroot does not hold:
 * one relative to AT_FDCWD after a chdir, one relative to an opened
 * directory descriptor, and a second level created through each new
 * directory. argv[1] holds "cwd/relative-parent" and "dirfd-parent".
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void fail(const char* operation, const char* path) {
  fprintf(stderr, "%s failed for %s: %s\n", operation, path, strerror(errno));
  exit(1);
}

static int open_directory(int dirfd, const char* path) {
  int fd = openat(dirfd, path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (fd < 0) {
    fail("openat", path);
  }
  return fd;
}

static void make_directory(int dirfd, const char* path) {
  if (mkdirat(dirfd, path, 0755) != 0) {
    fail("mkdirat", path);
  }
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s <pre-existing directory>\n", argv[0]);
    return 2;
  }
  int base = open_directory(AT_FDCWD, argv[1]);

  if (fchdir(base) != 0 || chdir("cwd") != 0) {
    fail("chdir", "cwd");
  }
  make_directory(AT_FDCWD, "relative-parent/at-fdcwd");
  make_directory(AT_FDCWD, "relative-parent/at-fdcwd/child");

  int parent = open_directory(base, "dirfd-parent");
  make_directory(parent, "at-dirfd");
  int created = open_directory(parent, "at-dirfd");
  make_directory(created, "child");

  close(created);
  close(parent);
  close(base);
  printf("mkdirat-beneath-record-time-directory-ok\n");
  return 0;
}
