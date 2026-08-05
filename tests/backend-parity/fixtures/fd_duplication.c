/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * File-descriptor duplication (fd-table) parity probe.
 *
 * A single process opens one temporary file and exercises the descriptor
 * duplication family against it, checking the POSIX invariants that Detcore's
 * fd model must preserve identically on every backend:
 *
 *   - dup(2) shares the underlying open file description, so a write through
 *     either descriptor advances the one shared file offset.
 *   - dup(2)/dup2(2) produce a target WITHOUT close-on-exec, regardless of the
 *     source's flag; dup3(2) with O_CLOEXEC and fcntl F_DUPFD_CLOEXEC produce a
 *     target WITH close-on-exec.
 *   - fcntl F_SETFD/F_GETFD round-trips FD_CLOEXEC on an existing descriptor.
 *   - fcntl F_DUPFD returns the lowest free descriptor at or above the request.
 *
 * Each duplication into an explicit target reuses a slot obtained with dup(2)
 * and immediately closed, so the probe never clobbers a descriptor the runtime
 * may hold. Only invariants (a byte offset and boolean checks) are observed --
 * never a raw descriptor number, which is free to differ across backends. The
 * observable is therefore an aggregate:
 *
 *   fd_dup offset=8 ok=11
 *
 * It is deliberately free of gated concerns: single process, no fork/thread,
 * and no pid, timestamp, cpu-time, or address is observed.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

/* Return a known-free descriptor number: dup the base, then close it. In a
 * single-threaded program the just-freed slot stays free until we reuse it. */
static int free_slot(int base) {
  int slot = dup(base);
  if (slot < 0)
    fail("dup (free_slot)");
  if (close(slot) != 0)
    fail("close (free_slot)");
  return slot;
}

static int cloexec_set(int fd) {
  int flags = fcntl(fd, F_GETFD);
  if (flags < 0)
    fail("fcntl F_GETFD");
  return (flags & FD_CLOEXEC) ? 1 : 0;
}

int main(void) {
  char template[] = "/tmp/fd_dup_XXXXXX";
  int base = mkstemp(template);
  if (base < 0)
    fail("mkstemp");
  /* Unlink now; the open descriptor keeps the file alive and the path is never
   * observed. */
  if (unlink(template) != 0)
    fail("unlink");

  int ok = 0;

  /* dup shares the file offset. */
  int dupfd = dup(base);
  if (dupfd < 0)
    fail("dup");
  if (write(base, "AAAA", 4) != 4)
    fail("write base");
  if (write(dupfd, "BBBB", 4) != 4)
    fail("write dup");
  off_t offset = lseek(base, 0, SEEK_CUR);
  if (offset == 8)
    ok++;
  if (lseek(dupfd, 0, SEEK_CUR) == 8)
    ok++;

  /* Fresh descriptors from dup default to no close-on-exec. */
  if (!cloexec_set(dupfd))
    ok++;

  /* F_SETFD/F_GETFD round-trips FD_CLOEXEC on the base descriptor. */
  if (!cloexec_set(base))
    ok++;
  if (fcntl(base, F_SETFD, FD_CLOEXEC) != 0)
    fail("fcntl F_SETFD");
  if (cloexec_set(base))
    ok++;

  /* dup2 into a freed slot returns that slot and clears close-on-exec. */
  int slot = free_slot(base);
  if (dup2(base, slot) == slot)
    ok++;
  if (!cloexec_set(slot))
    ok++;
  if (close(slot) != 0)
    fail("close dup2 slot");

  /* dup3 with O_CLOEXEC into a freed slot sets close-on-exec. */
  int slot3 = free_slot(base);
  if (dup3(base, slot3, O_CLOEXEC) == slot3)
    ok++;
  if (cloexec_set(slot3))
    ok++;
  if (close(slot3) != 0)
    fail("close dup3 slot");

  /* F_DUPFD returns the lowest free descriptor >= the requested minimum. */
  int high = fcntl(base, F_DUPFD, 100);
  if (high < 0)
    fail("fcntl F_DUPFD");
  if (high >= 100)
    ok++;
  if (close(high) != 0)
    fail("close F_DUPFD");

  /* F_DUPFD_CLOEXEC honors the minimum and sets close-on-exec. */
  int high_cloexec = fcntl(base, F_DUPFD_CLOEXEC, 100);
  if (high_cloexec < 0)
    fail("fcntl F_DUPFD_CLOEXEC");
  if (high_cloexec >= 100 && cloexec_set(high_cloexec))
    ok++;
  if (close(high_cloexec) != 0)
    fail("close F_DUPFD_CLOEXEC");

  if (close(dupfd) != 0)
    fail("close dup");
  if (close(base) != 0)
    fail("close base");

  printf("fd_dup offset=%ld ok=%d\n", (long)offset, ok);
  return 0;
}
