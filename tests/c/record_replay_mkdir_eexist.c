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
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

static void fail(const char *operation, const char *path) {
  fprintf(stderr, "%s failed for %s: errno=%d\n", operation, path, errno);
  exit(1);
}

static void expect_mkdir_errno(const char *path, int expected) {
  errno = 0;
  if (mkdir(path, 0777) != -1 || errno != expected) {
    fail("mkdir errno assertion", path);
  }
}

static void expect_mkdirat_errno(int dirfd, const char *path, int expected) {
  errno = 0;
  if (mkdirat(dirfd, path, 0777) != -1 || errno != expected) {
    fail("mkdirat errno assertion", path);
  }
}

static void expect_directory(const char *path) {
  if (chdir(path) != 0) {
    fail("chdir expected directory", path);
  }
  if (chdir("/") != 0) {
    fail("restore cwd", "/");
  }
}

static void expect_not_directory(const char *path) {
  errno = 0;
  if (chdir(path) == 0) {
    fail("chdir unexpectedly accepted non-directory", path);
  }
}

int main(int argc, char **argv) {
  if (argc != 12) {
    fprintf(stderr, "usage: %s BASIC_ROOT DIR FILE LINK WALK_ROOT WALK_LINK "
                    "WALK_PATH UNCONFINED_ROOT ABSOLUTE_DIR NEW_DIR "
                    "MISSING_CHILD\n",
            argv[0]);
    return 2;
  }

  expect_mkdir_errno(argv[1], EEXIST);
  expect_mkdir_errno(argv[2], EEXIST);
  expect_directory(argv[2]);

  expect_mkdir_errno(argv[3], EEXIST);
  expect_not_directory(argv[3]);
  expect_mkdir_errno(argv[4], EEXIST);
  expect_not_directory(argv[4]);

  if (mkdir(argv[10], 0777) != 0) {
    fail("mkdir expected success", argv[10]);
  }
  expect_directory(argv[10]);
  expect_mkdir_errno(argv[11], ENOENT);
  expect_not_directory(argv[11]);

  expect_mkdir_errno(argv[5], EEXIST);
  if (symlinkat("real/deep", AT_FDCWD, argv[6]) != 0) {
    fail("symlinkat expected success", argv[6]);
  }
  expect_mkdir_errno(argv[7], EEXIST);
  expect_directory(argv[7]);

  int dirfd = open(argv[8], O_PATH | O_CLOEXEC);
  if (dirfd < 0) {
    fail("open unconfined directory", argv[8]);
  }
  expect_mkdirat_errno(dirfd, "relative-existing", EEXIST);
  expect_mkdirat_errno(dirfd, argv[9], EEXIST);
  expect_directory(argv[9]);
  if (close(dirfd) != 0) {
    fail("close unconfined directory", argv[8]);
  }

  puts("mkdir-eexist-replay-ok");
  return 0;
}
