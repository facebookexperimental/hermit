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

#ifndef SYS_acct
#define SYS_acct 163
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_acct, NULL);
  if (result == -1 && errno == EPERM) {
    puts("acct-refused-ok");
    return 0;
  }

  fprintf(stderr, "acct: expected EPERM, got result=%ld errno=%d\n", result,
          errno);
  return 1;
}
