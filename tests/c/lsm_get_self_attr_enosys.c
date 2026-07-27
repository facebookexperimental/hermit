/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_lsm_get_self_attr
#define SYS_lsm_get_self_attr 459
#endif

#define LSM_ATTR_CURRENT 100U

int main(void) {
  size_t size = 0;
  errno = 0;
  long result =
      syscall(SYS_lsm_get_self_attr, LSM_ATTR_CURRENT, NULL, &size, 0U);
  if (result != -1 || errno != ENOSYS) {
    fprintf(stderr,
            "lsm_get_self_attr returned %ld with errno %d (%s), expected "
            "ENOSYS\n",
            result, errno, strerror(errno));
    return 1;
  }
  puts("lsm_get_self_attr deterministically unavailable");
  return 0;
}
