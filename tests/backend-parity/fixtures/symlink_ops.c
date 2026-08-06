/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Symbolic-link parity probe: symlinkat / readlinkat / lstat vs stat.
 *
 * A single process creates a regular target file and a symlink to it under a
 * unique temporary root, then checks the deterministic Linux symlink rules every
 * backend's file model must honor identically. Every observation is a pure
 * function of the process's own path operations, with no dependence on time,
 * scheduling, pid, or host identity:
 *
 *   - symlinkat creates a link whose readlinkat contents equal the exact target
 *     path string that was stored.
 *   - lstat of the link reports type S_IFLNK with a size equal to the stored
 *     target path length (lstat does not follow the link).
 *   - stat of the link follows it and reports the target's regular-file type and
 *     its six-byte size.
 *   - opening the link reads the target's contents (checksum 597 over "abcdef").
 *   - a dangling symlink to a missing path still lstat's as S_IFLNK, while stat
 *     through it fails with ENOENT.
 *
 * Only invariants are printed:
 *
 *   symlink_ops size=6 checksum=597 ok=6
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

#define PAYLOAD "abcdef"
#define PAYLOAD_LEN 6

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

int main(void) {
  char root[] = "/tmp/symlink_ops_XXXXXX";
  if (mkdtemp(root) == NULL)
    fail("mkdtemp");

  char target[128];
  char link[128];
  char dangling[128];
  snprintf(target, sizeof(target), "%s/target", root);
  snprintf(link, sizeof(link), "%s/link", root);
  snprintf(dangling, sizeof(dangling), "%s/dead", root);

  int target_fd = open(target, O_WRONLY | O_CREAT | O_EXCL, 0600);
  if (target_fd < 0)
    fail("open target");
  if (write(target_fd, PAYLOAD, PAYLOAD_LEN) != PAYLOAD_LEN)
    fail("write payload");
  if (close(target_fd) != 0)
    fail("close target");

  int root_fd = open(root, O_RDONLY | O_DIRECTORY);
  if (root_fd < 0)
    fail("open root");

  int ok = 0;
  struct stat st;

  /* symlinkat creates a link whose readlinkat contents equal the target path. */
  char readback[128];
  if (symlinkat(target, root_fd, "link") == 0) {
    ssize_t n = readlinkat(root_fd, "link", readback, sizeof(readback) - 1);
    if (n > 0) {
      readback[n] = '\0';
      if (strcmp(readback, target) == 0)
        ok++;
    }
  }
  if (close(root_fd) != 0)
    fail("close root fd");

  /* lstat does not follow the link: type S_IFLNK, size = target path length. */
  if (lstat(link, &st) == 0 && (st.st_mode & S_IFMT) == S_IFLNK &&
      st.st_size == (off_t)strlen(target))
    ok++;

  /* stat follows the link to the regular target of the written size. */
  if (stat(link, &st) == 0 && (st.st_mode & S_IFMT) == S_IFREG &&
      st.st_size == PAYLOAD_LEN)
    ok++;

  /* Opening the link reads the target's payload. */
  char buf[PAYLOAD_LEN];
  long checksum = 0;
  int link_fd = open(link, O_RDONLY);
  if (link_fd >= 0) {
    if (read(link_fd, buf, PAYLOAD_LEN) == PAYLOAD_LEN) {
      for (size_t i = 0; i < PAYLOAD_LEN; i++)
        checksum += (unsigned char)buf[i];
      ok++;
    }
    if (close(link_fd) != 0)
      fail("close link fd");
  }

  /* A dangling symlink still lstat's as a link. */
  if (symlink("/tmp/symlink_ops_absent_target", dangling) != 0)
    fail("symlink dangling");
  if (lstat(dangling, &st) == 0 && (st.st_mode & S_IFMT) == S_IFLNK)
    ok++;

  /* stat through the dangling link fails with ENOENT. */
  errno = 0;
  if (stat(dangling, &st) < 0 && errno == ENOENT)
    ok++;

  long size = (long)PAYLOAD_LEN;

  if (unlink(dangling) != 0)
    fail("unlink dangling");
  if (unlink(link) != 0)
    fail("unlink link");
  if (unlink(target) != 0)
    fail("unlink target");
  if (rmdir(root) != 0)
    fail("rmdir root");

  printf("symlink_ops size=%ld checksum=%ld ok=%d\n", size, checksum, ok);
  return 0;
}
