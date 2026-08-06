/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Directory lifecycle parity probe: mkdirat / rmdir semantics.
 *
 * A single process builds and tears down a small directory tree under a unique
 * temporary root and checks the deterministic Linux directory rules every
 * backend's file model must honor identically. Every observation is a pure
 * function of the process's own path operations, with no dependence on time,
 * scheduling, pid, or host identity:
 *
 *   - mkdir creates a directory that stat reports with type S_IFDIR.
 *   - mkdir of an existing path fails with EEXIST.
 *   - a nested child directory is created with mkdirat relative to the parent.
 *   - rmdir of a non-empty directory fails with ENOTEMPTY.
 *   - after the child is removed, rmdir of the parent succeeds.
 *   - stat of the removed parent then fails with ENOENT.
 *
 * Only invariants are printed:
 *
 *   mkdir_rmdir ok=6
 *
 * It is deliberately free of gated concerns: single process, no fork/thread, and
 * no pid, timestamp, cpu-time, inode, device, uid, gid, or address is observed.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

int main(void) {
  char root[] = "/tmp/mkdir_rmdir_XXXXXX";
  if (mkdtemp(root) == NULL)
    fail("mkdtemp");

  char parent[128];
  snprintf(parent, sizeof(parent), "%s/parent", root);

  int ok = 0;
  struct stat st;

  /* mkdir creates a directory that stat reports as S_IFDIR. */
  if (mkdir(parent, 0700) == 0 && stat(parent, &st) == 0 &&
      (st.st_mode & S_IFMT) == S_IFDIR)
    ok++;

  /* mkdir of an existing path fails with EEXIST. */
  errno = 0;
  if (mkdir(parent, 0700) < 0 && errno == EEXIST)
    ok++;

  /* A nested child directory is created with mkdirat relative to the parent. */
  int parent_fd = open(parent, O_RDONLY | O_DIRECTORY);
  if (parent_fd < 0)
    fail("open parent");
  if (mkdirat(parent_fd, "child", 0700) == 0)
    ok++;
  if (close(parent_fd) != 0)
    fail("close parent fd");

  /* rmdir of a non-empty directory fails with ENOTEMPTY. */
  errno = 0;
  if (rmdir(parent) < 0 && errno == ENOTEMPTY)
    ok++;

  /* Remove the child, then rmdir of the now-empty parent succeeds. */
  char child[192];
  snprintf(child, sizeof(child), "%s/child", parent);
  if (rmdir(child) != 0)
    fail("rmdir child");
  if (rmdir(parent) == 0)
    ok++;

  /* stat of the removed parent fails deterministically with ENOENT. */
  errno = 0;
  if (stat(parent, &st) < 0 && errno == ENOENT)
    ok++;

  if (rmdir(root) != 0)
    fail("rmdir root");

  printf("mkdir_rmdir ok=%d\n", ok);
  return 0;
}
