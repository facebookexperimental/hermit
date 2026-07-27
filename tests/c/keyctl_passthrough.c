/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Reports whether `keyctl(KEYCTL_GET_KEYRING_ID, ...)` returned ENOSYS,
 * without asserting a particular outcome. Its purpose is a host-independent
 * parity check: run natively and again under non-strict `hermit run` and
 * compare the reported flag. Non-strict Hermit must pass the keyring syscall
 * through to the host (matching native), rather than forcing the deterministic
 * ENOSYS boundary that only applies to strict/fail-closed mode.
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
  int is_enosys = (result == -1 && errno == ENOSYS) ? 1 : 0;
  printf("keyctl_enosys=%d\n", is_enosys);
  return 0;
}
