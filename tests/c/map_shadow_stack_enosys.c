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

#ifndef SYS_map_shadow_stack
#define SYS_map_shadow_stack 453
#endif

int main(void) {
  errno = 0;
  long result = syscall(SYS_map_shadow_stack, 0UL, 4096UL, 0U);
  if (result != -1 || errno != ENOSYS) {
    fprintf(
        stderr,
        "map_shadow_stack returned %ld with errno %d (%s), expected ENOSYS\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }
  puts("map_shadow_stack deterministically unavailable");
  return 0;
}
