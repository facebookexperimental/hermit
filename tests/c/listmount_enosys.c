/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_listmount
#define SYS_listmount 458
#endif

int main(void) {
  struct {
    uint32_t size;
    uint32_t spare;
    uint64_t mount_id;
    uint64_t last_mount_id;
    uint64_t mount_namespace_id;
  } request = {.size = sizeof(request), .mount_id = 1};
  uint64_t mount_ids[64];

  errno = 0;
  long result = syscall(SYS_listmount, &request, mount_ids, 64, 0);
  if (result != -1 || errno != ENOSYS) {
    fprintf(
        stderr,
        "listmount returned %ld with errno %d (%s), expected ENOSYS\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }
  puts("listmount deterministically unavailable");
  return 0;
}
