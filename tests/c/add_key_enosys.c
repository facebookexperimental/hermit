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
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_add_key
#define SYS_add_key 248
#endif

#define KEY_SPEC_PROCESS_KEYRING -2

int main(void) {
  errno = 0;
  long result = syscall(SYS_add_key, "user", "hermit-add-key", "x", 1,
                        KEY_SPEC_PROCESS_KEYRING);
  if (result == -1 && errno == ENOSYS) {
    puts("add_key deterministically unavailable");
    return 0;
  }

  fprintf(stderr, "add_key: expected ENOSYS, got result=%ld errno=%d\n",
          result, errno);
  return 1;
}
