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
#include <sys/ptrace.h>

int main(void) {
  errno = 0;
  long result = ptrace(PTRACE_TRACEME, 0, NULL, NULL);
  if (result != -1 || errno != EPERM) {
    fprintf(stderr, "PTRACE_TRACEME returned %ld with errno %d (%s), expected EPERM\n",
            result, errno, strerror(errno));
    return 1;
  }

  puts("PTRACE_TRACEME deterministically refused");
  return 0;
}
