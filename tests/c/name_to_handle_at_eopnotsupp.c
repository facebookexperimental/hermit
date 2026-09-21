/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_name_to_handle_at
#define SYS_name_to_handle_at 303
#endif

int main(void) {
  size_t storage_size = sizeof(struct file_handle) + 128;
  struct file_handle *handle = calloc(1, storage_size);
  if (handle == NULL) {
    perror("allocate file handle");
    return 2;
  }
  handle->handle_bytes = 128;

  int mount_id = 0;
  errno = 0;
  long result = syscall(SYS_name_to_handle_at, AT_FDCWD, "/", handle,
                        &mount_id, 0U);
  if (result != -1 || errno != EOPNOTSUPP) {
    fprintf(stderr,
            "name_to_handle_at returned %ld with errno %d (%s), expected "
            "EOPNOTSUPP\n",
            result, errno, strerror(errno));
    free(handle);
    return 1;
  }
  free(handle);
  puts("name_to_handle_at deterministically refused");
  return 0;
}
