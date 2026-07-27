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

#ifndef SYS_keyctl
#define SYS_keyctl 250
#endif

#define KEYCTL_GET_KEYRING_ID 0
#define KEY_SPEC_SESSION_KEYRING -3

int main(void) {
  errno = 0;
  long result = syscall(SYS_keyctl, KEYCTL_GET_KEYRING_ID,
                        KEY_SPEC_SESSION_KEYRING, 0, 0, 0);
  if (result == -1 && errno == ENOSYS) {
    puts("keyctl deterministically unavailable");
    return 0;
  }

  fprintf(stderr, "keyctl: expected ENOSYS, got result=%ld errno=%d\n",
          result, errno);
  return 1;
}
