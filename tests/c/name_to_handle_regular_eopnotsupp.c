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

int main(void) {
  struct file_handle *handle = calloc(1, sizeof(*handle) + 128);
  if (handle == NULL) {
    perror("calloc file_handle");
    return 1;
  }
  int mount_id = -1;
  handle->handle_bytes = 128;

  errno = 0;
  int result =
      name_to_handle_at(AT_FDCWD, "/etc/hosts", handle, &mount_id, 0);
  int call_errno = errno;
  free(handle);
  if (result != -1 || call_errno != EOPNOTSUPP) {
    fprintf(stderr,
            "regular name_to_handle_at returned %d with errno %d (%s), "
            "expected EOPNOTSUPP\n",
            result, call_errno, strerror(call_errno));
    return 1;
  }

  puts("regular name_to_handle_at deterministically refused");
  return 0;
}
