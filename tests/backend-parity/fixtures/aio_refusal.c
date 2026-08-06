/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

/*
 * The kernel AIO interface (io_setup/io_submit/io_getevents/io_destroy, distinct
 * from io_uring) completes asynchronous I/O out of program order and exposes
 * host-timing-dependent completion events. Hermit refuses the whole subsystem
 * deterministically with ENOSYS rather than admitting that nondeterminism into
 * the guest, mirroring the io_uring and copy_file_range contracts. This row
 * asserts that every AIO entry point fails identically with ENOSYS across all
 * three backends and that the refused io_setup leaves the caller's context id
 * untouched, even though io_setup succeeds outside Hermit.
 */

int main(void) {
  int ok = 0;

  unsigned long ctx = 0;
  errno = 0;
  long result = syscall(SYS_io_setup, 128, &ctx);
  if (result == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(
        stderr,
        "io_setup returned %ld errno %d (%s), expected ENOSYS\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }

  /* The refusal must not have handed back a usable context id. */
  if (ctx == 0) {
    ok++;
  } else {
    fprintf(stderr, "io_setup populated ctx=%lu on refusal\n", ctx);
    return 1;
  }

  errno = 0;
  result = syscall(SYS_io_submit, (unsigned long)0x1234, 0L, (void *)0);
  if (result == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(
        stderr,
        "io_submit returned %ld errno %d (%s), expected ENOSYS\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }

  errno = 0;
  result = syscall(
      SYS_io_getevents, (unsigned long)0x1234, 0L, 0L, (void *)0, (void *)0);
  if (result == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(
        stderr,
        "io_getevents returned %ld errno %d (%s), expected ENOSYS\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }

  errno = 0;
  result = syscall(SYS_io_destroy, (unsigned long)0x1234);
  if (result == -1 && errno == ENOSYS) {
    ok++;
  } else {
    fprintf(
        stderr,
        "io_destroy returned %ld errno %d (%s), expected ENOSYS\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }

  printf("aio ok=%d\n", ok);
  return 0;
}
